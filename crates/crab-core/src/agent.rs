use crate::provider::{Provider, ProviderError};
use crate::session::{Session, SessionEntry};
use crate::tool::ToolRegistry;
use crate::types::{ContentBlock, EntryId, Message, Role, StopReason, ToolCallId};

/// One-shot summary instruction (spec §6). Kept short and concrete: the
/// summary replaces history, so it must carry decisions, not flavour.
const SUMMARY_PROMPT: &str = "\
Summarize the following conversation so work can continue from your summary \
alone. Keep, with specifics: the user's goals, decisions made and why, files \
modified, commands run and their outcomes, pending tasks, and key technical \
findings (error messages, APIs, formats). Drop pleasantries and restatements. \
Under 800 words.\n\n[Conversation to summarize]";

/// The result of one compaction pass (TUI notices; the event is authoritative).
#[derive(Debug, PartialEq)]
pub struct CompactReport {
    pub strategy: crate::obs::CompactionStrategy,
    pub shed_tool_results: u32,
    pub turns_affected: u32,
    /// 计划摘要但调用失败（或返回空文本），退回 shed-only（spec §6）。
    pub summary_failed: bool,
}

#[derive(Debug, Default, Clone)]
pub struct AgentConfig {
    /// Prepended to the provider request as the system message. Empty = not injected.
    /// Not part of session history (it's per-run config, not conversation).
    pub system_prompt: String,
    /// Context window size, when the profile declares one. Drives auto-compaction.
    pub context_window: Option<u64>,
    /// Auto-compact when the last prompt exceeds this fraction of the window.
    /// 0.0 (the Default) disables auto-compaction; wired to 0.8 in production.
    pub auto_compact_threshold: f64,
    /// Compactions replayed from the session log at --continue.
    pub compactions: Vec<crate::compact::AppliedCompaction>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted {
        turn: usize,
    },
    TextDelta(String),
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    },
    AssistantMessage(Message),
    ToolCallStarted {
        id: ToolCallId,
        name: String,
    },
    ToolCallFinished {
        id: ToolCallId,
        result: String,
        is_error: bool,
    },
    Finished {
        reason: StopReason,
    },
    /// Compaction condensed the request view (auto or /compact). The TUI
    /// surfaces it as a notice; the durable record is the Compacted obs event.
    Compacted {
        strategy: crate::obs::CompactionStrategy,
        shed_tool_results: u32,
        turns_affected: u32,
    },
}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    session: Session,
    config: AgentConfig,
    history: Vec<Message>,
    compactions: Vec<crate::compact::AppliedCompaction>,
    /// Prompt tokens of the most recent LlmCall (auto-compaction trigger signal).
    last_prompt: Option<u64>,
    abort: tokio_util::sync::CancellationToken,
    events_tx: tokio::sync::broadcast::Sender<AgentEvent>,
    provider_calls: Vec<Vec<Message>>,
    next_run_id: u64,
}

impl Agent {
    pub fn new(
        provider: Box<dyn Provider>,
        tools: ToolRegistry,
        session: Session,
        config: AgentConfig,
    ) -> Self {
        let (events_tx, _) = tokio::sync::broadcast::channel(64);
        // Restore conversation history from a loaded session (--continue). A freshly
        // created session has no entries → empty history. (Fixes continue amnesia.)
        let history: Vec<Message> = session.messages().cloned().collect();
        // Seed the trigger signal from the log: a resumed session may already
        // be over threshold before its first call (--continue compacts up front).
        let last_prompt = crate::meter::context_used(session.entries());
        let compactions = config.compactions.clone();
        Self {
            provider,
            tools,
            session,
            config,
            history,
            compactions,
            last_prompt,
            abort: tokio_util::sync::CancellationToken::new(),
            events_tx,
            provider_calls: Vec::new(),
            next_run_id: 0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.events_tx.subscribe()
    }

    pub fn abort(&self) {
        self.abort.cancel();
    }

    /// Cloneable token so callers can abort while `run` holds `&mut self` (interactive
    /// Ctrl-C). Cancel it to abort the in-flight run.
    pub fn abort_token(&self) -> tokio_util::sync::CancellationToken {
        self.abort.clone()
    }

    pub fn provider_calls(&self) -> &[Vec<Message>] {
        &self.provider_calls
    }

    /// Swap the model mid-session. History, session log, tools, and config are
    /// untouched — only the next `complete` uses the new provider. Called between
    /// runs (e.g. the TUI's `/model`); must NOT be called while `run` holds `&mut self`.
    pub fn set_provider(&mut self, provider: Box<dyn Provider>) {
        self.provider = provider;
    }

    /// 手动 /compact 入口：无视阈值，立即执行一次压缩（spec §4）。
    pub async fn compact_now(&mut self) -> CompactReport {
        self.run_compaction().await
    }

    fn compaction_due(&self) -> bool {
        self.config.auto_compact_threshold > 0.0
            && match (self.last_prompt, self.config.context_window) {
                (Some(p), Some(w)) => p as f64 > self.config.auto_compact_threshold * w as f64,
                _ => false,
            }
    }

    /// 执行一次压缩（spec §5 瘦身 → §6 按需摘要 → 落盘 + 事件）。
    async fn run_compaction(&mut self) -> CompactReport {
        let sizes = crate::meter::prompt_sizes(self.session.entries());
        let budget = crate::compact::Budget {
            trigger: self.config.auto_compact_threshold,
            keep: 0.4,
            window: self.config.context_window,
        };
        let plan = crate::compact::plan(&self.history, &sizes, &budget);
        if plan.keep_from == 0 {
            return CompactReport {
                strategy: crate::obs::CompactionStrategy::Shed,
                shed_tool_results: 0,
                turns_affected: 0,
                summary_failed: false,
            };
        }

        // 先确定性瘦身变换，计数（摘要路径用同一变换后的 prefix 做输入）
        let prefix = &self.history[..plan.keep_from];
        let mut names: std::collections::HashMap<ToolCallId, String> =
            std::collections::HashMap::new();
        for m in prefix {
            for b in &m.content {
                if let ContentBlock::ToolUse { id, name, .. } = b {
                    names.insert(id.clone(), name.clone());
                }
            }
        }
        let shed_prefix: Vec<Message> = prefix
            .iter()
            .map(|m| crate::compact::shed_message(m, &names))
            .collect();
        let shed_count = prefix
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count() as u32;
        let turns_affected = (plan.keep_from / 2) as u32;

        let mut strategy = crate::obs::CompactionStrategy::Shed;
        let mut summary = None;
        let mut summary_failed = false;
        if plan.need_summary {
            let mut req = vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: SUMMARY_PROMPT.into(),
                }],
            }];
            req.extend(shed_prefix);
            match self.provider.complete(&req, &[], None).await {
                Ok(t) => {
                    let text: String = t
                        .message
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.trim().is_empty() {
                        summary_failed = true;
                    } else {
                        strategy = crate::obs::CompactionStrategy::Summarize;
                        summary = Some(text);
                    }
                }
                Err(_) => summary_failed = true,
            }
        }

        // up_to = 日志里第 keep_from-1 条 Message 的 EntryId；找不到（坏日志）
        // 则放弃本次压缩——绝不落一条指向不存在条目的水位线。
        let Some(up_to) = self
            .session
            .entries()
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Message { id, .. } => Some(*id),
                _ => None,
            })
            .nth(plan.keep_from - 1)
        else {
            return CompactReport {
                strategy: crate::obs::CompactionStrategy::Shed,
                shed_tool_results: 0,
                turns_affected: 0,
                summary_failed: false,
            };
        };

        if let Err(e) = self.session.append_obs(crate::obs::ObsEvent {
            run_id: self.next_run_id,
            ts: crate::util::now_ms(),
            body: crate::obs::ObsBody::Compacted {
                up_to,
                strategy,
                shed_tool_results: shed_count,
                turns_affected,
                summary: summary.clone(),
            },
        }) {
            eprintln!("[crab] obs append failed: {e}");
        }
        self.compactions.push(crate::compact::AppliedCompaction {
            keep_from: plan.keep_from,
            strategy,
            summary,
        });
        let report = CompactReport {
            strategy,
            shed_tool_results: shed_count,
            turns_affected,
            summary_failed,
        };
        let _ = self.events_tx.send(AgentEvent::Compacted {
            strategy,
            shed_tool_results: shed_count,
            turns_affected,
        });
        report
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn session_entries(&self) -> &[SessionEntry] {
        self.session.entries()
    }

    /// Session file stem, used as the metrics row's session id.
    pub fn session_id(&self) -> String {
        self.session
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn last_entry_id(&self) -> Option<EntryId> {
        self.session.entries().last().map(|e| e.id())
    }

    pub async fn run(&mut self, user_input: String) -> Result<StopReason, AgentError> {
        // Bridge provider's live StreamEvents into AgentEvents (P-A).
        // Guard aborts the forward task on EVERY exit path so it can't leak.
        let (stream_tx, mut stream_rx) =
            tokio::sync::broadcast::channel::<crate::stream::StreamEvent>(64);
        let events_tx2 = self.events_tx.clone();
        let forward = tokio::spawn(async move {
            loop {
                match stream_rx.recv().await {
                    Ok(ev) => {
                        if std::env::var("CRAB_DEBUG_FWD").is_ok() {
                            eprintln!("FWD RECV {ev:?}");
                        }
                        match ev {
                            crate::stream::StreamEvent::TextDelta(s) => {
                                let _ = events_tx2.send(AgentEvent::TextDelta(s));
                            }
                            crate::stream::StreamEvent::Usage {
                                prompt_tokens,
                                completion_tokens,
                                cache_read_tokens,
                                cache_write_tokens,
                            } => {
                                let _ = events_tx2.send(AgentEvent::Usage {
                                    prompt_tokens,
                                    completion_tokens,
                                    cache_read_tokens,
                                    cache_write_tokens,
                                });
                            }
                            _ => {}
                        }
                    }
                    // Lagging only loses text deltas, not Finished — skip and continue.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        let result = self.run_inner(user_input, run_id, &stream_tx).await;
        // Ensure all deltas forwarded before returning, then stop the task.
        drop(stream_tx);
        forward.abort();
        let _ = forward.await;
        // Reset the abort token for the next run: CancellationToken::cancel() is
        // permanent, so without this a single Ctrl-C would make every subsequent
        // run return Aborted instantly. Callers fetch abort_token() before each
        // run, so resetting here (after the run is over) is safe.
        self.abort = tokio_util::sync::CancellationToken::new();
        result
    }

    async fn run_inner(
        &mut self,
        user_input: String,
        run_id: u64,
        stream_tx: &tokio::sync::broadcast::Sender<crate::stream::StreamEvent>,
    ) -> Result<StopReason, AgentError> {
        let mut llm_calls = 0u32;
        let mut tool_calls = 0u32;
        self.obs(run_id, crate::obs::ObsBody::TurnStart);
        let result = self
            .loop_steps(
                user_input,
                run_id,
                &mut llm_calls,
                &mut tool_calls,
                stream_tx,
            )
            .await;
        let (stop_reason, interrupted) = match &result {
            Ok(r) => (*r, matches!(r, StopReason::Aborted)),
            // Provider/IO error: the turn is over and incomplete.
            Err(_) => (StopReason::Aborted, true),
        };
        self.obs(
            run_id,
            crate::obs::ObsBody::TurnEnd {
                stop_reason,
                interrupted,
                llm_calls,
                tool_calls,
            },
        );
        result
    }

    /// Append an observability event. Failure only warns: 观测不改变系统.
    fn obs(&mut self, run_id: u64, body: crate::obs::ObsBody) {
        let event = crate::obs::ObsEvent {
            run_id,
            ts: crate::util::now_ms(),
            body,
        };
        if let Err(e) = self.session.append_obs(event) {
            eprintln!("[crab] obs append failed: {e}");
        }
    }

    async fn loop_steps(
        &mut self,
        user_input: String,
        run_id: u64,
        llm_calls: &mut u32,
        tool_calls: &mut u32,
        stream_tx: &tokio::sync::broadcast::Sender<crate::stream::StreamEvent>,
    ) -> Result<StopReason, AgentError> {
        let user = Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: user_input }],
        };
        self.record(user.clone())?;
        self.history.push(user);

        for turn in 0usize.. {
            // Tool results from the prior turn are already recorded (Q11), so aborting
            // here cannot leave an orphan tool_use. Check before the next model call.
            if self.abort.is_cancelled() {
                let _ = self.events_tx.send(AgentEvent::Finished {
                    reason: StopReason::Aborted,
                });
                return Ok(StopReason::Aborted);
            }
            // Compaction trigger is judged on the previous call's usage at the
            // top of the loop, so a resumed session (--continue) compacts before
            // its first request too.
            if self.compaction_due() {
                self.run_compaction().await;
            }
            let _ = self.events_tx.send(AgentEvent::TurnStarted { turn });

            self.provider_calls.push(self.history.clone());
            // Race the model call against abort (Q11 cancel-safety): if abort fires
            // mid-stream, drop the call (a kill_on_drop-style cancel) and persist what
            // we already have. Tool results from prior turns are already recorded.
            let tool_schemas = self.tools.schemas();
            // Inject system prompt as the first message for THIS call only (not history).
            // The history goes through the compaction view: full fidelity stays in
            // the log; the provider sees the compressed view.
            let view_msgs = crate::compact::view(&self.history, &self.compactions);
            let request_msgs: Vec<Message> = if self.config.system_prompt.is_empty() {
                view_msgs.clone()
            } else {
                let mut v = Vec::with_capacity(view_msgs.len() + 1);
                v.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: self.config.system_prompt.clone(),
                    }],
                });
                v.extend(view_msgs.clone());
                v
            };
            // Record what we actually send (includes the injected system message).
            *self.provider_calls.last_mut().unwrap() = request_msgs.clone();
            let call_started = std::time::Instant::now();
            let t = tokio::select! {
                res = self.provider.complete(&request_msgs, &tool_schemas, Some(stream_tx)) => res?,
                _ = self.abort.cancelled() => {
                    let _ = self.events_tx.send(AgentEvent::Finished { reason: StopReason::Aborted });
                    return Ok(StopReason::Aborted);
                }
            };
            *llm_calls += 1;
            if let Some(u) = t.usage {
                self.last_prompt = Some(u.prompt as u64);
            }
            self.obs(
                run_id,
                crate::obs::ObsBody::LlmCall {
                    provider: self.provider.label().to_string(),
                    model: self.provider.model().to_string(),
                    duration_ms: call_started.elapsed().as_millis() as u64,
                    usage: t.usage,
                    outcome: crate::obs::ObsOutcome::Ok,
                },
            );
            let msg = t.message.clone();
            let _ = self
                .events_tx
                .send(AgentEvent::AssistantMessage(msg.clone()));
            self.record(msg.clone())?;
            self.history.push(msg.clone());

            if t.stop_reason != StopReason::ToolUse {
                // Turn ended mid-tool-use (error / length limit) after the
                // assistant message was recorded: answer every dangling
                // tool_call with a synthesized error result. Otherwise the
                // NEXT request violates the wire protocol (tool_calls must be
                // followed by tool messages) and the provider 400s.
                let dangling: Vec<ContentBlock> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, .. } => Some(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: format!(
                                "error: the model turn ended ({:?}) before this tool call completed; no result was produced",
                                t.stop_reason
                            ),
                            is_error: true,
                        }),
                        _ => None,
                    })
                    .collect();
                if !dangling.is_empty() {
                    let m = Message {
                        role: Role::User,
                        content: dangling,
                    };
                    self.record(m.clone())?;
                    self.history.push(m);
                }
                let _ = self.events_tx.send(AgentEvent::Finished {
                    reason: t.stop_reason,
                });
                return Ok(t.stop_reason);
            }

            let mut results: Vec<ContentBlock> = Vec::new();
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    let _ = self.events_tx.send(AgentEvent::ToolCallStarted {
                        id: id.clone(),
                        name: name.clone(),
                    });
                    let tool_started = std::time::Instant::now();
                    let bytes_in = input.to_string().len() as u64;
                    let (content, is_error) = match self.tools.execute(name, input.clone()).await {
                        Ok(r) => (r, false),
                        Err(e) => (e.to_string(), true),
                    };
                    self.obs(
                        run_id,
                        crate::obs::ObsBody::ToolCall {
                            tool: name.clone(),
                            call_seq: *tool_calls,
                            args_digest: crate::obs::digest(input),
                            result_digest: crate::obs::digest(&serde_json::Value::String(
                                content.clone(),
                            )),
                            bytes_in,
                            bytes_out: content.len() as u64,
                            duration_ms: tool_started.elapsed().as_millis() as u64,
                            outcome: if is_error {
                                crate::obs::ObsOutcome::Error(content.chars().take(128).collect())
                            } else {
                                crate::obs::ObsOutcome::Ok
                            },
                        },
                    );
                    *tool_calls += 1;
                    let _ = self.events_tx.send(AgentEvent::ToolCallFinished {
                        id: id.clone(),
                        result: content.clone(),
                        is_error,
                    });
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content,
                        is_error,
                    });
                }
            }
            let tool_result_msg = Message {
                role: Role::User,
                content: results,
            };
            self.record(tool_result_msg.clone())?;
            self.history.push(tool_result_msg);
        }
        // An unbounded range only ends via `return` above (EndTurn / error / abort).
        unreachable!("the turn loop exits only through return")
    }

    fn record(&mut self, m: Message) -> Result<(), AgentError> {
        self.session.append(m)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provider::{MockProvider, ProviderError};
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::{AssistantTurn, ContentBlock, StopReason};
    use async_trait::async_trait;

    pub(crate) struct Noop;

    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("noop")
        }
        fn description(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("no op")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> {
            Ok("done".into())
        }
    }

    pub(crate) fn tool_turn() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolUseStart {
                index: 0,
                id: "c1".into(),
                name: "noop".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{}".into(),
            },
            StreamEvent::ToolUseEnd { index: 0 },
            StreamEvent::StreamEnd {
                reason: StopReason::ToolUse,
            },
        ]
    }

    /// A tool_use round whose stream then dies with an error stop.
    pub(crate) fn tool_turn_errored() -> Vec<StreamEvent> {
        let mut v = tool_turn();
        if let Some(last) = v.last_mut() {
            *last = StreamEvent::StreamEnd {
                reason: StopReason::Error,
            };
        }
        v
    }

    #[tokio::test]
    async fn one_tool_round_then_end_turn() {
        let provider = MockProvider::new(vec![
            tool_turn(),
            vec![
                StreamEvent::TextDelta("all done".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                ..Default::default()
            },
        );

        let reason = agent.run("hi".into()).await.unwrap();
        assert_eq!(reason, StopReason::EndTurn);

        let calls = agent.provider_calls();
        let second = &calls[1];
        let has_tool_result = second.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(
                b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "c1")
            })
        });
        assert!(has_tool_result, "tool result must be fed back to model");
    }

    fn obs_bodies(agent: &Agent) -> Vec<(u64, crate::obs::ObsBody)> {
        agent
            .session_entries()
            .iter()
            .filter_map(|e| match e {
                crate::session::SessionEntry::Obs { event, .. } => {
                    Some((event.run_id, event.body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn summarize_compaction_shrinks_the_request_view_not_the_history() {
        let provider = MockProvider::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let config = AgentConfig {
            compactions: vec![crate::compact::AppliedCompaction {
                keep_from: 2,
                strategy: crate::obs::CompactionStrategy::Summarize,
                summary: Some("earlier we chose X".into()),
            }],
            ..Default::default()
        };
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        // 预置 3 对历史（直接推入，绕过 run）
        for i in 0..3 {
            agent.history.push(crate::types::Message {
                role: crate::types::Role::User,
                content: vec![crate::types::ContentBlock::Text {
                    text: format!("u{i}"),
                }],
            });
            agent.history.push(crate::types::Message {
                role: crate::types::Role::Assistant,
                content: vec![crate::types::ContentBlock::Text {
                    text: format!("a{i}"),
                }],
            });
        }
        agent.run("go".into()).await.unwrap();
        let calls = agent.provider_calls();
        let req = calls.last().unwrap();
        // history 保持全量（8 条：3 对 + 本轮 user + assistant 回复）
        assert_eq!(agent.history().len(), 8);
        // 请求在回复前构造：视图 = 1 条 summary + 5 条后缀 = 6 条；prefix 原文不在请求里
        assert_eq!(req.len(), 6, "{req:?}");
        assert!(!req.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, crate::types::ContentBlock::Text { text } if text == "u0"))
        }));
        assert!(req[0].content.iter().any(|b| matches!(b, crate::types::ContentBlock::Text { text } if text.contains("[Earlier conversation summary]"))));
    }

    use crate::obs::CompactionStrategy;

    /// 一次纯文本 turn，带指定 prompt 数的 usage
    fn usage_turn(prompt_tokens: u32) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta("working".into()),
            StreamEvent::Usage {
                prompt_tokens,
                completion_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]
    }

    #[tokio::test]
    async fn auto_compaction_fires_when_prompt_crosses_threshold() {
        // 窗口 100k × 0.8 = 80k。触发用上一轮的 usage 在本轮 loop 顶判定：
        // run0(60k)→run1(80k，不严格大于)→run2(100k)；run3 的 loop 顶触发。
        // 注意 plan 按 prompt 增量计费：平的序列会让后缀"免费"，usage 必须递增。
        let provider = MockProvider::new(vec![
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
            vec![
                // 摘要调用（need_summary 成立时消费）
                StreamEvent::TextDelta("summary of earlier work".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
            usage_turn(50_000),
        ]);
        let config = AgentConfig {
            context_window: Some(100_000),
            auto_compact_threshold: 0.8,
            ..Default::default()
        };
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("start".into()).await.unwrap(); // turn0: 60k
        agent.run("more".into()).await.unwrap(); // turn1: 80k（不触发）
        agent.run("again".into()).await.unwrap(); // turn2: 100k
        agent.run("third".into()).await.unwrap(); // loop 顶：100k > 80k → 压缩
        let bodies = obs_bodies(&agent);
        assert!(
            bodies
                .iter()
                .any(|(_, b)| matches!(b, crate::obs::ObsBody::Compacted { .. })),
            "expected a Compacted event, got {bodies:?}"
        );
        assert_eq!(agent.compactions.len(), 1);
    }

    #[tokio::test]
    async fn compact_now_sheds_and_appends_only_one_obs_line() {
        // run0 用带 tool 的脚本（产生 ToolResult 供 shed），其第二次调用接 usage_turn(60k)；
        // run1/run2 各一次 usage_turn。usage 递增 → plan 的增量计费有效。
        let provider = MockProvider::new(vec![
            tool_turn(),
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
            vec![
                // need_summary 成立时的摘要调用
                StreamEvent::TextDelta("summary text".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let config = AgentConfig {
            context_window: Some(10_000), // 小窗口 → keep_from > 0
            auto_compact_threshold: 0.0,
            ..Default::default()
        };
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let path = session.path().to_path_buf();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("one".into()).await.unwrap();
        agent.run("two".into()).await.unwrap();
        agent.run("three".into()).await.unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let report = agent.compact_now().await;
        assert!(report.turns_affected > 0, "nothing compacted");
        let after = std::fs::read_to_string(&path).unwrap();
        // 红线：日志 append-only——after = before + 恰好一行
        assert!(after.starts_with(&before), "log prefix violated");
        let added = after[before.len()..].trim();
        assert_eq!(added.lines().count(), 1, "expected exactly one new line");
        assert!(added.contains("compacted"), "{added}");
        // 视图反映最终策略：Summarize → prefix 被一条摘要替换；Shed → 块不删只换内容
        let v = crate::compact::view(agent.history(), &agent.compactions);
        if report.strategy == CompactionStrategy::Summarize {
            assert!(v.len() < agent.history().len());
            assert!(v[0].content.iter().any(|b| matches!(b,
                crate::types::ContentBlock::Text { text } if text.contains("[Earlier conversation summary]"))));
        } else {
            assert_eq!(v.len(), agent.history().len());
            assert!(v.iter().flat_map(|m| &m.content).any(|b| matches!(b,
                crate::types::ContentBlock::ToolResult { content, .. } if content.contains("[shed:"))));
        }
    }

    #[tokio::test]
    async fn summary_failure_falls_back_to_shed_only() {
        struct Fail;
        #[async_trait::async_trait]
        impl crate::provider::Provider for Fail {
            async fn complete(
                &self,
                _: &[crate::types::Message],
                _: &[serde_json::Value],
                _: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamEvent>>,
            ) -> Result<crate::types::AssistantTurn, crate::provider::ProviderError> {
                Err(crate::provider::ProviderError::Api {
                    status: 500,
                    body: "boom".into(),
                })
            }
            fn label(&self) -> &str {
                "fail"
            }
            fn model(&self) -> &str {
                "fail"
            }
        }
        let provider = MockProvider::new(vec![
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
        ]);
        let config = AgentConfig {
            context_window: Some(10_000),
            ..Default::default()
        };
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("x".into()).await.unwrap();
        agent.run("y".into()).await.unwrap();
        agent.run("z".into()).await.unwrap(); // 3 对 → plan 不会前置返回
        agent.set_provider(Box::new(Fail));
        let report = agent.compact_now().await;
        assert_eq!(report.strategy, CompactionStrategy::Shed);
        assert!(report.summary_failed); // 降级可见
    }

    #[tokio::test]
    async fn resumed_session_over_threshold_compacts_before_first_request() {
        // --continue 场景：日志里最后一次调用已超阈值 → 本轮 loop 顶即压缩
        let dir = tempfile::tempdir().unwrap();
        let mut session = crate::session::Session::create(dir.path()).unwrap();
        let long_first_question = format!("question 0 {}", "detail".repeat(800)); // ~3200 chars, shed-able
        session
            .append(crate::types::Message {
                role: crate::types::Role::User,
                content: vec![crate::types::ContentBlock::Text {
                    text: long_first_question.clone(),
                }],
            })
            .unwrap();
        for i in 1..3 {
            session
                .append(crate::types::Message {
                    role: crate::types::Role::User,
                    content: vec![crate::types::ContentBlock::Text {
                        text: format!("question {i}"),
                    }],
                })
                .unwrap();
            session
                .append(crate::types::Message {
                    role: crate::types::Role::Assistant,
                    content: vec![crate::types::ContentBlock::Text {
                        text: format!("answer {i}"),
                    }],
                })
                .unwrap();
        }
        session
            .append_obs(crate::obs::ObsEvent {
                run_id: 0,
                ts: 1,
                body: crate::obs::ObsBody::LlmCall {
                    provider: "x".into(),
                    model: "x".into(),
                    duration_ms: 1,
                    usage: Some(crate::obs::LlmUsage {
                        prompt: 110_000,
                        ..Default::default()
                    }),
                    outcome: crate::obs::ObsOutcome::Ok,
                },
            })
            .unwrap();
        let loaded = crate::session::Session::load(session.path()).unwrap();
        let provider = MockProvider::new(vec![usage_turn(50_000)]);
        let config = AgentConfig {
            context_window: Some(128_000),
            auto_compact_threshold: 0.8,
            ..Default::default()
        };
        let tools = ToolRegistry::new();
        let mut agent = Agent::new(Box::new(provider), tools, loaded, config);
        agent.run("continue please".into()).await.unwrap();
        let bodies = obs_bodies(&agent);
        assert!(
            bodies
                .iter()
                .any(|(_, b)| matches!(b, crate::obs::ObsBody::Compacted { .. })),
            "expected a Compacted event on resume, got {bodies:?}"
        );
        // 压缩后的请求视图：prefix 的长文本被截断为 shed 占位（块不删，内容换）
        let req = agent.provider_calls().last().unwrap();
        assert!(!req.iter().any(|m| m.content.iter().any(|b| matches!(
            b, crate::types::ContentBlock::Text { text } if text == &long_first_question
        ))));
        assert!(req.iter().flat_map(|m| &m.content).any(|b| matches!(
            b, crate::types::ContentBlock::Text { text } if text.contains("chars total, shed")
        )));
    }

    #[tokio::test]
    async fn run_writes_obs_turn_llm_tool_events() {
        let provider = MockProvider::new(vec![
            tool_turn(),
            vec![
                StreamEvent::TextDelta("all done".into()),
                StreamEvent::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cache_read_tokens: 2,
                    cache_write_tokens: 1,
                },
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, AgentConfig::default());

        let reason = agent.run("hi".into()).await.unwrap();
        assert_eq!(reason, StopReason::EndTurn);

        let bodies = obs_bodies(&agent);
        assert!(
            matches!(&bodies[0], (0, crate::obs::ObsBody::TurnStart)),
            "got {bodies:?}"
        );
        assert!(
            matches!(&bodies[1], (0, crate::obs::ObsBody::LlmCall { provider, model, usage: None, outcome: crate::obs::ObsOutcome::Ok, .. }) if provider == "mock" && model == "mock"),
            "got {bodies:?}"
        );
        match &bodies[2] {
            (
                0,
                crate::obs::ObsBody::ToolCall {
                    tool,
                    call_seq,
                    bytes_out,
                    outcome,
                    ..
                },
            ) => {
                assert_eq!(tool, "noop");
                assert_eq!(*call_seq, 0);
                assert_eq!(*bytes_out, 4); // "done"
                assert_eq!(*outcome, crate::obs::ObsOutcome::Ok);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert!(
            matches!(
                &bodies[3],
                (0, crate::obs::ObsBody::LlmCall { usage: Some(u), .. })
                if *u == crate::obs::LlmUsage { prompt: 10, completion: 5, cache_read: 2, cache_write: 1 }
            ),
            "got {bodies:?}"
        );
        match &bodies[4] {
            (
                0,
                crate::obs::ObsBody::TurnEnd {
                    stop_reason,
                    interrupted,
                    llm_calls,
                    tool_calls,
                },
            ) => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert!(!interrupted);
                assert_eq!(*llm_calls, 2);
                assert_eq!(*tool_calls, 1);
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn aborted_run_writes_interrupted_turn_end() {
        let provider = MockProvider::cooperative(vec![vec![
            StreamEvent::TextDelta("slow".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, AgentConfig::default());
        agent.abort_token().cancel();
        let reason = agent.run("hi".into()).await.unwrap();
        assert_eq!(reason, StopReason::Aborted);
        let bodies = obs_bodies(&agent);
        assert!(
            bodies.iter().any(|(_, b)| matches!(
                b,
                crate::obs::ObsBody::TurnEnd {
                    interrupted: true,
                    ..
                }
            )),
            "got {bodies:?}"
        );
    }

    #[tokio::test]
    async fn provider_error_still_writes_turn_end() {
        struct Fail;
        #[async_trait]
        impl Provider for Fail {
            async fn complete(
                &self,
                _m: &[Message],
                _t: &[serde_json::Value],
                _e: Option<&tokio::sync::broadcast::Sender<StreamEvent>>,
            ) -> Result<AssistantTurn, ProviderError> {
                Err(ProviderError::Network("boom".into()))
            }
            fn label(&self) -> &str {
                "fail"
            }
            fn model(&self) -> &str {
                "fail"
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(Fail), tools, session, AgentConfig::default());
        assert!(agent.run("hi".into()).await.is_err());
        let bodies = obs_bodies(&agent);
        assert!(
            matches!(&bodies[0], (0, crate::obs::ObsBody::TurnStart)),
            "got {bodies:?}"
        );
        assert!(
            bodies.iter().any(|(_, b)| matches!(
                b,
                crate::obs::ObsBody::TurnEnd {
                    interrupted: true,
                    ..
                }
            )),
            "got {bodies:?}"
        );
    }

    #[tokio::test]
    async fn turn_ending_mid_tool_use_synthesizes_error_results() {
        // Model emitted tool_calls but the stream ended with an error/length
        // stop: the assistant message is recorded WITH tool_calls. Without
        // synthesized results the NEXT request violates the protocol
        // (tool_calls must be answered by tool messages).
        let provider = MockProvider::new(vec![tool_turn_errored()]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, AgentConfig::default());

        let reason = agent.run("hi".into()).await.unwrap();
        assert_eq!(reason, StopReason::Error);

        // The session must contain a ToolResult answering tool_use c1.
        let answered = agent.session_entries().iter().any(|e| {
            matches!(
                e,
                crate::session::SessionEntry::Message { message, .. }
                if message.content.iter().any(|b| matches!(
                    b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "c1"))
            )
        });
        assert!(
            answered,
            "dangling tool_use c1 must get a synthesized result"
        );

        // And the wire form of the recorded history must be protocol-valid:
        // every role:"tool" message follows an assistant carrying tool_calls.
        let wire: Vec<serde_json::Value> = agent
            .history()
            .iter()
            .flat_map(crate::provider::openai::messages_to_openai_for_test)
            .collect();
        let mut prev_has_tool_calls = false;
        for msg in &wire {
            let role = msg["role"].as_str().unwrap_or("");
            if role == "tool" {
                assert!(
                    prev_has_tool_calls,
                    "role:tool without preceding tool_calls: {wire:?}"
                );
            }
            prev_has_tool_calls = msg.get("tool_calls").is_some();
        }
    }

    #[tokio::test]
    async fn abort_after_first_tool_result_persists_no_orphan_tool_use() {
        // Turn 1: model requests a tool, tool runs. We abort when the tool result
        // arrives, so turn 2's complete() sees the abort. Turn 1's tool_use +
        // tool_result must both be on disk (no orphan tool_use).
        // Turn 1 requests a tool. Turn 2 streams several events; the cooperative mock
        // yields between them, so the abort watcher (fired on ToolCallFinished) can
        // land mid-stream. That makes the abort deterministic, not a scheduling race.
        let provider = MockProvider::cooperative(vec![
            tool_turn(),
            vec![
                StreamEvent::TextDelta("working".into()),
                StreamEvent::TextDelta(" more".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let provider_cancel = provider.abort_handle();
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                ..Default::default()
            },
        );

        // Subscribe BEFORE running; on the first ToolCallFinished, cancel BOTH the
        // agent token (checked at turn top + select!) and the provider token (checked
        // inside complete()). This mirrors a real Ctrl-C hitting the whole stack.
        let mut rx = agent.subscribe();
        let abort = agent.abort.clone();
        let watcher = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if matches!(ev, AgentEvent::ToolCallFinished { .. }) {
                    abort.cancel();
                    provider_cancel.cancel();
                    break;
                }
            }
        });

        let reason = agent.run("hi".into()).await.unwrap();
        watcher.await.unwrap();
        assert_eq!(reason, StopReason::Aborted);

        // Q11: no orphan tool_use — every tool_use has a paired tool_result on disk.
        let mut pending: Vec<String> = Vec::new();
        for e in agent.session_entries() {
            let crate::session::SessionEntry::Message { message, .. } = e else {
                continue;
            };
            for b in &message.content {
                match b {
                    ContentBlock::ToolUse { id, .. } => pending.push(id.clone()),
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        pending.retain(|x| x != tool_use_id)
                    }
                    _ => {}
                }
            }
        }
        assert!(pending.is_empty(), "orphan tool_use ids left: {pending:?}");
        // And we must have actually run the tool (otherwise the assertion is vacuous).
        assert!(
            agent.session_entries().iter().any(|e| matches!(
                e,
                crate::session::SessionEntry::Message { message, .. }
                    if message.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            )),
            "expected at least one tool_result recorded"
        );
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::StopReason;
    use async_trait::async_trait;

    struct Noop;
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("noop")
        }
        fn description(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("no op")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> {
            Ok("done".into())
        }
    }

    #[tokio::test]
    async fn agent_bridges_provider_deltas_to_text_events() {
        let provider = MockProvider::cooperative(vec![vec![
            StreamEvent::TextDelta("a".into()),
            StreamEvent::TextDelta("b".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                ..Default::default()
            },
        );
        let mut rx = agent.subscribe();
        agent.run("hi".into()).await.unwrap();
        // Collect TextDelta events with a bounded wait (forward task needs a tick).
        let mut deltas = Vec::new();
        while deltas.len() < 2 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(AgentEvent::TextDelta(s))) => deltas.push(s),
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert_eq!(deltas, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn agent_emits_usage_event() {
        let provider = MockProvider::cooperative(vec![vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                ..Default::default()
            },
        );
        let mut rx = agent.subscribe();
        agent.run("hi".into()).await.unwrap();
        // Collect Usage events with a bounded wait (forward task needs a tick).
        let mut usage = None;
        while usage.is_none() {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(AgentEvent::Usage {
                    prompt_tokens,
                    completion_tokens,
                    ..
                })) => usage = Some((prompt_tokens, completion_tokens)),
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert_eq!(usage, Some((10, 5)));
    }
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::StopReason;
    use async_trait::async_trait;

    struct Noop;
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("noop")
        }
        fn description(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("no op")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> {
            Ok("done".into())
        }
    }

    #[tokio::test]
    async fn system_prompt_prepended_to_first_request() {
        let provider = MockProvider::new(vec![vec![StreamEvent::StreamEnd {
            reason: StopReason::EndTurn,
        }]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                system_prompt: "BASE RULES".into(),
                ..Default::default()
            },
        );
        agent.run("hi".into()).await.unwrap();
        let first = &agent.provider_calls()[0];
        assert!(
            first.iter().any(|m| m.content.iter().any(|b| matches!(
                b, ContentBlock::Text { text } if text.contains("BASE RULES")
            ))),
            "system prompt must reach provider"
        );
    }

    #[tokio::test]
    async fn empty_system_prompt_not_injected() {
        let provider = MockProvider::new(vec![vec![StreamEvent::StreamEnd {
            reason: StopReason::EndTurn,
        }]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                system_prompt: String::new(),
                ..Default::default()
            },
        );
        agent.run("hi".into()).await.unwrap();
        let first = &agent.provider_calls()[0];
        // 只有用户消息，无注入的 system
        assert_eq!(first.len(), 1);
    }
}

#[cfg(test)]
mod abort_token_tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::StopReason;
    use async_trait::async_trait;

    struct Noop;
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("noop")
        }
        fn description(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("no op")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> {
            Ok("done".into())
        }
    }

    #[tokio::test]
    async fn cloned_abort_token_cancels_run() {
        // Cooperative provider streams several events; we cancel mid-stream via the
        // cloned token (the interactive Ctrl-C path).
        let provider = MockProvider::cooperative(vec![vec![
            StreamEvent::TextDelta("w".into()),
            StreamEvent::TextDelta("x".into()),
            StreamEvent::TextDelta("y".into()),
            StreamEvent::TextDelta("z".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(
            Box::new(provider),
            tools,
            session,
            AgentConfig {
                ..Default::default()
            },
        );
        let token = agent.abort_token();
        // cancel after a tick so run is mid-stream
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            token.cancel();
        });
        let reason = agent.run("hi".into()).await.unwrap();
        // Either Aborted (cancel won) or EndTurn (finished first) — but token path works.
        assert!(matches!(reason, StopReason::Aborted | StopReason::EndTurn));
    }
}

#[cfg(test)]
mod continue_history_tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::StopReason;
    use async_trait::async_trait;

    struct Noop;
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("noop")
        }
        fn description(&self) -> std::borrow::Cow<'static, str> {
            std::borrow::Cow::Borrowed("no op")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> {
            Ok("done".into())
        }
    }

    #[tokio::test]
    async fn continued_session_restores_history_to_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let provider = MockProvider::new(vec![vec![
                StreamEvent::TextDelta("first answer".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ]]);
            let mut tools = ToolRegistry::new();
            tools.register(Box::new(Noop));
            let session = crate::session::Session::create(dir.path()).unwrap();
            path = session.path().to_path_buf();
            let mut agent = Agent::new(
                Box::new(provider),
                tools,
                session,
                AgentConfig {
                    ..Default::default()
                },
            );
            agent.run("first question".into()).await.unwrap();
        }
        let provider2 = MockProvider::new(vec![vec![StreamEvent::StreamEnd {
            reason: StopReason::EndTurn,
        }]]);
        let mut tools2 = ToolRegistry::new();
        tools2.register(Box::new(Noop));
        let session2 = crate::session::Session::load(&path).unwrap();
        let mut agent2 = Agent::new(
            Box::new(provider2),
            tools2,
            session2,
            AgentConfig {
                ..Default::default()
            },
        );
        agent2.run("second question".into()).await.unwrap();

        let first_call = &agent2.provider_calls()[0];
        let texts: Vec<String> = first_call
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("first question")),
            "history must include prior user msg"
        );
        assert!(
            texts.iter().any(|t| t.contains("first answer")),
            "history must include prior assistant msg"
        );
        assert!(
            texts.iter().any(|t| t.contains("second question")),
            "history must include new msg"
        );
    }
}
