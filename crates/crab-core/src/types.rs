use serde::{Deserialize, Serialize};

pub type ToolCallId = String;
pub type EntryId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: ToolCallId,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Aborted,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTurn {
    pub message: Message,
    pub stop_reason: StopReason,
    pub interrupted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_roundtrips() {
        let b = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "a.rs"}),
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn content_block_tag_is_snake_case() {
        let b = ContentBlock::Text { text: "hi".into() };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"type":"text","text":"hi"}"#
        );
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            r#""end_turn""#
        );
    }
}
