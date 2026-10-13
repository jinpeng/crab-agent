use crate::stream::{StreamAccumulator, StreamEvent};
use crate::types::{AssistantTurn, Message};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub mod auth;
pub mod openai;
pub mod sse;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("network: {0}")]
    Network(String),
    #[error("rate limited / server error: {0}")]
    Retryable(String),
    #[error("aborted")]
    Aborted,
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// `tools` are the JSON schemas of currently-available tools (per-request, since
    /// the set can change as MCP/extension tools come and go). Cancellation-aware.
    /// `on_event`: optional channel the provider pushes StreamEvents into AS THEY
    /// ARRIVE (for live terminal rendering). The returned AssistantTurn is unchanged —
    /// streaming is a side channel (P-A).
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
        on_event: Option<&tokio::sync::broadcast::Sender<StreamEvent>>,
    ) -> Result<AssistantTurn, ProviderError>;
}

/// Test provider: each `complete` pops the next scripted event sequence.
pub struct MockProvider {
    scripts: Mutex<Vec<Vec<StreamEvent>>>,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
    cancel: CancellationToken,
    /// When true, yield to the scheduler between events so a concurrent abort can
    /// interleave mid-stream (mimics a real provider awaiting network IO).
    cooperative: bool,
}

impl MockProvider {
    pub fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        // Reverse to a stack so pop() replays in order.
        Self {
            scripts: Mutex::new(scripts.into_iter().rev().collect()),
            calls: Arc::new(Mutex::new(Vec::new())),
            cancel: CancellationToken::new(),
            cooperative: false,
        }
    }

    /// A mock that yields between events, so abort can land mid-stream.
    pub fn cooperative(scripts: Vec<Vec<StreamEvent>>) -> Self {
        let mut s = Self::new(scripts);
        s.cooperative = true;
        s
    }

    pub fn calls(&self) -> Vec<Vec<Message>> {
        self.calls.lock().unwrap().clone()
    }

    pub fn abort_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[serde_json::Value],
        on_event: Option<&tokio::sync::broadcast::Sender<StreamEvent>>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.calls.lock().unwrap().push(messages.to_vec());
        let events = self.scripts.lock().unwrap().pop().unwrap_or_default();
        let mut acc = StreamAccumulator::new();
        for ev in events {
            if self.cancel.is_cancelled() {
                acc.push(StreamEvent::StreamEnd {
                    reason: crate::types::StopReason::Aborted,
                });
                break;
            }
            if let Some(tx) = on_event {
                let _ = tx.send(ev.clone());
            }
            acc.push(ev);
            if self.cooperative {
                tokio::task::yield_now().await;
            }
        }
        Ok(acc.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamEvent;
    use crate::types::{Message, Role, StopReason};

    #[tokio::test]
    async fn mock_replays_scripted_events_per_call() {
        let mock = MockProvider::new(vec![
            vec![
                StreamEvent::TextDelta("hello".into()),
                StreamEvent::StreamEnd {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            }],
        ]);
        let msgs: Vec<Message> = vec![Message {
            role: Role::User,
            content: vec![],
        }];
        let t1 = mock.complete(&msgs, &[], None).await.unwrap();
        let t2 = mock.complete(&msgs, &[], None).await.unwrap();
        assert_eq!(t1.stop_reason, StopReason::EndTurn);
        assert_eq!(t2.stop_reason, StopReason::EndTurn);
        assert_eq!(mock.calls().len(), 2);
    }

    #[tokio::test]
    async fn mock_abort_produces_interrupted_turn() {
        let mock = MockProvider::new(vec![vec![
            StreamEvent::TextDelta("a".into()),
            StreamEvent::TextDelta("b".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let cancel = mock.abort_handle();
        cancel.cancel();
        let t = mock.complete(&[], &[], None).await.unwrap();
        assert!(t.interrupted);
    }

    #[tokio::test]
    async fn mock_streams_events_to_on_event_channel() {
        let mock = MockProvider::cooperative(vec![vec![
            StreamEvent::TextDelta("a".into()),
            StreamEvent::TextDelta("b".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn,
            },
        ]]);
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _ = mock.complete(&[], &[], Some(&tx)).await.unwrap();
        drop(tx);
        let mut got = Vec::new();
        while let Ok(e) = rx.recv().await {
            got.push(e);
        }
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(s) if s == "a"))
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::TextDelta(s) if s == "b"))
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, StreamEvent::StreamEnd { .. }))
        );
    }

    #[tokio::test]
    async fn complete_without_on_event_still_works() {
        let mock = MockProvider::new(vec![vec![StreamEvent::StreamEnd {
            reason: StopReason::EndTurn,
        }]]);
        let t = mock.complete(&[], &[], None).await.unwrap();
        assert_eq!(t.stop_reason, StopReason::EndTurn);
    }
}
