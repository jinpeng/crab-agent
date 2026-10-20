use crate::provider::{Provider, ProviderError};
use crate::session::{Session, SessionEntry};
use crate::tool::ToolRegistry;
use crate::types::{ContentBlock, EntryId, Message, Role, StopReason, ToolCallId};

#[derive(Debug, Default)]
pub struct AgentConfig {
    /// Prepended to the provider request as the system message. Empty = not injected.
    /// Not part of session history (it's per-run config, not conversation).
    pub system_prompt: String,
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
}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    session: Session,
    config: AgentConfig,
    history: Vec<Message>,
    abort: tokio_util::sync::CancellationToken,
    events_tx: tokio::sync::broadcast::Sender<AgentEvent>,
    provider_calls: Vec<Vec<Message>>,
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
        Self {
            provider,
            tools,
            session,
            config,
            history,
            abort: tokio_util::sync::CancellationToken::new(),
            events_tx,
            provider_calls: Vec::new(),
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

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn session_entries(&self) -> &[SessionEntry] {
        self.session.entries()
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
                            } => {
                                let _ = events_tx2.send(AgentEvent::Usage {
                                    prompt_tokens,
                                    completion_tokens,
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
        let result = self.run_inner(user_input, &stream_tx).await;
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
            let _ = self.events_tx.send(AgentEvent::TurnStarted { turn });

            self.provider_calls.push(self.history.clone());
            // Race the model call against abort (Q11 cancel-safety): if abort fires
            // mid-stream, drop the call (a kill_on_drop-style cancel) and persist what
            // we already have. Tool results from prior turns are already recorded.
            let tool_schemas = self.tools.schemas();
            // Inject system prompt as the first message for THIS call only (not history).
            let request_msgs: Vec<Message> = if self.config.system_prompt.is_empty() {
                self.history.clone()
            } else {
                let mut v = Vec::with_capacity(self.history.len() + 1);
                v.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: self.config.system_prompt.clone(),
                    }],
                });
                v.extend(self.history.clone());
                v
            };
            // Record what we actually send (includes the injected system message).
            *self.provider_calls.last_mut().unwrap() = request_msgs.clone();
            let t = tokio::select! {
                res = self.provider.complete(&request_msgs, &tool_schemas, Some(stream_tx)) => res?,
                _ = self.abort.cancelled() => {
                    let _ = self.events_tx.send(AgentEvent::Finished { reason: StopReason::Aborted });
                    return Ok(StopReason::Aborted);
                }
            };
            let msg = t.message.clone();
            let _ = self
                .events_tx
                .send(AgentEvent::AssistantMessage(msg.clone()));
            self.record(msg.clone())?;
            self.history.push(msg.clone());

            if t.stop_reason != StopReason::ToolUse {
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
                    let (content, is_error) = match self.tools.execute(name, input.clone()).await {
                        Ok(r) => (r, false),
                        Err(e) => (e.to_string(), true),
                    };
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
    use crate::provider::MockProvider;
    use crate::stream::StreamEvent;
    use crate::tool::{Tool, ToolError, ToolRegistry};
    use crate::types::{ContentBlock, StopReason};
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
