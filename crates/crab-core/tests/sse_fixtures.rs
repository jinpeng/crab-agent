use crab_core::provider::sse::parse_chunk;
use crab_core::stream::StreamAccumulator;
use crab_core::types::{ContentBlock, StopReason};

/// Split an SSE byte stream into data payloads (mimics eventsource-stream output).
fn payloads(sse: &str) -> Vec<String> {
    sse.split("\n\n")
        .filter_map(|frame| {
            let line = frame.lines().find(|l| l.starts_with("data:"))?;
            Some(line.trim_start_matches("data:").trim().to_string())
        })
        .collect()
}

fn run_fixture(sse: &str) -> crab_core::types::AssistantTurn {
    let mut acc = StreamAccumulator::new();
    for p in payloads(sse) {
        for ev in parse_chunk(&p).unwrap() {
            acc.push(ev);
        }
    }
    acc.finish()
}

#[test]
fn openai_text_fixture() {
    let t = run_fixture(include_str!("fixtures/openai_text.sse"));
    assert_eq!(t.stop_reason, StopReason::EndTurn);
    assert!(!t.interrupted);
    match &t.message.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Hello, world"),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn openai_tool_fixture_assembles_arguments() {
    let t = run_fixture(include_str!("fixtures/openai_tool.sse"));
    assert_eq!(t.stop_reason, StopReason::ToolUse);
    match &t.message.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_9");
            assert_eq!(name, "read");
            assert_eq!(*input, serde_json::json!({"path": "x"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn openai_tool_noindex_fixture_degrades_to_last_slot() {
    let t = run_fixture(include_str!("fixtures/openai_tool_noindex.sse"));
    assert_eq!(t.stop_reason, StopReason::ToolUse);
    match &t.message.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_k");
            assert_eq!(name, "grep");
            assert_eq!(*input, serde_json::json!({"q": "fn"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}
