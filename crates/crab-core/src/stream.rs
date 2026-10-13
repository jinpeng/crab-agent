use crate::types::{AssistantTurn, ContentBlock, Message, Role, StopReason, ToolCallId};

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        index: usize,
        id: ToolCallId,
        name: String,
    },
    ToolInputDelta {
        index: usize,
        json_fragment: String,
    },
    ToolUseEnd {
        index: usize,
    },
    StreamEnd {
        reason: StopReason,
    },
}

struct ToolSlot {
    id: ToolCallId,
    name: String,
    json_buf: String,
}

/// Assembles streaming events into an AssistantTurn. Pure state machine, no IO (Q13).
/// Slots keyed by index; same-index deltas append; arguments parsed only when complete.
pub struct StreamAccumulator {
    text: String,
    /// (index, slot); output sorted by index for stable ordering.
    slots: Vec<(usize, ToolSlot)>,
    stop: StopReason,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            slots: Vec::new(),
            stop: StopReason::EndTurn,
        }
    }

    pub fn push(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::TextDelta(s) => self.text.push_str(&s),
            StreamEvent::ToolUseStart { index, id, name } => {
                self.slots.push((
                    index,
                    ToolSlot {
                        id,
                        name,
                        json_buf: String::new(),
                    },
                ));
            }
            StreamEvent::ToolInputDelta {
                index,
                json_fragment,
            } => {
                if let Some((_, slot)) = self.slot_mut(index) {
                    slot.json_buf.push_str(&json_fragment);
                }
            }
            StreamEvent::ToolUseEnd { .. } => {}
            StreamEvent::StreamEnd { reason } => self.stop = reason,
        }
    }

    fn slot_mut(&mut self, index: usize) -> Option<&mut (usize, ToolSlot)> {
        // Exact index match first; else degrade to "append to last slot" (providers
        // that omit index, Q13).
        let pos = self
            .slots
            .iter()
            .position(|(i, _)| *i == index)
            .or(if self.slots.is_empty() {
                None
            } else {
                Some(self.slots.len() - 1)
            });
        pos.map(|p| &mut self.slots[p])
    }

    pub fn finish(mut self) -> AssistantTurn {
        self.slots.sort_by_key(|(i, _)| *i);
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: std::mem::take(&mut self.text),
            });
        }
        for (_, slot) in self.slots {
            match serde_json::from_str::<serde_json::Value>(&slot.json_buf) {
                Ok(input) => content.push(ContentBlock::ToolUse {
                    id: slot.id,
                    name: slot.name,
                    input,
                }),
                // Parse failure: surface raw string as an error ToolResult so the model retries.
                Err(e) => content.push(ContentBlock::ToolResult {
                    tool_use_id: slot.id,
                    content: format!("invalid tool arguments JSON: {e}. raw: {}", slot.json_buf),
                    is_error: true,
                }),
            }
        }
        let interrupted = matches!(self.stop, StopReason::Aborted | StopReason::Error);
        AssistantTurn {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: self.stop,
            interrupted,
        }
    }
}

impl Default for StreamAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Role, StopReason};

    fn run(events: Vec<StreamEvent>) -> AssistantTurn {
        let mut sm = StreamAccumulator::new();
        for e in events {
            sm.push(e);
        }
        sm.finish()
    }

    #[test]
    fn assembles_text_and_single_tool_call() {
        let t = run(vec![
            StreamEvent::TextDelta("let me ".into()),
            StreamEvent::TextDelta("read".into()),
            StreamEvent::ToolUseStart {
                index: 0,
                id: "t1".into(),
                name: "read".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{\"pa".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "th\":\"x\"}".into(),
            },
            StreamEvent::ToolUseEnd { index: 0 },
            StreamEvent::StreamEnd {
                reason: StopReason::ToolUse,
            },
        ]);
        assert_eq!(t.stop_reason, StopReason::ToolUse);
        assert!(!t.interrupted);
        assert_eq!(t.message.role, Role::Assistant);
        assert_eq!(t.message.content.len(), 2);
        match &t.message.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read");
                assert_eq!(*input, serde_json::json!({"path": "x"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn out_of_order_indices_build_separate_slots() {
        let t = run(vec![
            StreamEvent::ToolUseStart {
                index: 1,
                id: "b".into(),
                name: "write".into(),
            },
            StreamEvent::ToolUseStart {
                index: 0,
                id: "a".into(),
                name: "read".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 1,
                json_fragment: "{}".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{}".into(),
            },
            StreamEvent::ToolUseEnd { index: 0 },
            StreamEvent::ToolUseEnd { index: 1 },
            StreamEvent::StreamEnd {
                reason: StopReason::ToolUse,
            },
        ]);
        let ids: Vec<_> = t
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn malformed_arguments_surfaced_as_error_tool_result_for_model_retry() {
        let t = run(vec![
            StreamEvent::ToolUseStart {
                index: 0,
                id: "t1".into(),
                name: "read".into(),
            },
            StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{bad json".into(),
            },
            StreamEvent::ToolUseEnd { index: 0 },
            StreamEvent::StreamEnd {
                reason: StopReason::ToolUse,
            },
        ]);
        match &t.message.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "t1");
                assert!(*is_error);
                assert!(content.contains("{bad json"));
            }
            other => panic!("expected error ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn abort_marks_interrupted_and_keeps_partial() {
        let t = run(vec![
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::StreamEnd {
                reason: StopReason::Aborted,
            },
        ]);
        assert!(t.interrupted);
        match &t.message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "partial"),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
