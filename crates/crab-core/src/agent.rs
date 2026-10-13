use crate::provider::{Provider, ProviderError};
use crate::session::{Session, SessionEntry};
use crate::tool::ToolRegistry;
use crate::types::{ContentBlock, EntryId, Message, Role, StopReason, ToolCallId};

#[derive(Debug)]
pub struct AgentConfig {
    pub max_turns: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("exceeded max_turns {0}")]
    MaxTurns(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted {
        turn: usize,
    },
    TextDelta(String),
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
        Self {
            provider,
            tools,
            session,
            config,
            history: Vec::new(),
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

    pub fn provider_calls(&self) -> &[Vec<Message>] {
        &self.provider_calls
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
        let user = Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: user_input }],
        };
        self.record(user.clone());
        self.history.push(user);

        for turn in 0..self.config.max_turns {
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
            let t = self.provider.complete(&self.history).await?;
            let msg = t.message.clone();
            let _ = self
                .events_tx
                .send(AgentEvent::AssistantMessage(msg.clone()));
            self.record(msg.clone());
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
            self.record(tool_result_msg.clone());
            self.history.push(tool_result_msg);
        }
        Err(AgentError::MaxTurns(self.config.max_turns))
    }

    fn record(&mut self, m: Message) {
        let _ = self.session.append(m);
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
        fn name(&self) -> &'static str {
            "noop"
        }
        fn description(&self) -> &'static str {
            "no op"
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
            AgentConfig { max_turns: 10 },
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
}
