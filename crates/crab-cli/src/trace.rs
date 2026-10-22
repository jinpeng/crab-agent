//! `crab trace` — render the span tree of a session file.
//!
//! Traces are DERIVED, not stored: obs events (timing/usage/outcome) join the
//! message log (tool args/results) by (run_id, call_seq) + digest (spec: 日志即真相).

use crab_core::obs::{LlmUsage, ObsBody, ObsOutcome};
use crab_core::session::{Session, SessionEntry};
use crab_core::types::ContentBlock;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpanNode {
    pub label: String,
    pub detail: String,
    pub duration_ms: Option<u64>,
    pub usage: Option<LlmUsage>,
    pub ok: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Trace {
    pub run_id: u64,
    pub spans: Vec<SpanNode>,
    pub complete: bool,
    pub start_ts: u64,
}

/// One pending tool_use from the message log, waiting for its obs ToolCall.
#[derive(Debug)]
struct PendingToolUse {
    name: String,
    digest: String,
    summary: String,
}

pub fn build_traces(entries: &[SessionEntry]) -> Vec<Trace> {
    let mut traces: Vec<Trace> = Vec::new();
    let mut current: Option<usize> = None; // index into traces
    let mut pending: Vec<PendingToolUse> = Vec::new();

    let open = |traces: &mut Vec<Trace>, current: &mut Option<usize>, run_id: u64, ts: u64| {
        traces.push(Trace {
            run_id,
            spans: Vec::new(),
            complete: false,
            start_ts: ts,
        });
        *current = Some(traces.len() - 1);
    };

    for entry in entries {
        match entry {
            SessionEntry::Obs { event, .. } => match &event.body {
                ObsBody::TurnStart => open(&mut traces, &mut current, event.run_id, event.ts),
                ObsBody::TurnEnd { .. } => {
                    if let Some(i) = current {
                        traces[i].complete = true;
                    }
                    current = None;
                    pending.clear();
                }
                ObsBody::LlmCall {
                    provider,
                    model,
                    duration_ms,
                    usage,
                    outcome,
                } => {
                    if let Some(i) = current {
                        let (ok, note) = outcome_parts(outcome);
                        let mut label = format!("llm {provider}/{model}");
                        if let Some(n) = note {
                            label.push_str(&format!(" [{n}]"));
                        }
                        traces[i].spans.push(SpanNode {
                            label,
                            detail: String::new(),
                            duration_ms: Some(*duration_ms),
                            usage: *usage,
                            ok,
                        });
                    }
                }
                ObsBody::ToolCall {
                    tool,
                    call_seq,
                    args_digest,
                    result_digest: _,
                    duration_ms,
                    outcome,
                    ..
                } => {
                    if let Some(i) = current {
                        let (ok, note) = outcome_parts(outcome);
                        let joined = pending.get(*call_seq as usize);
                        let detail = match joined {
                            Some(p) if p.digest == *args_digest => {
                                format!("{} {}", p.name, p.summary)
                            }
                            Some(_) => format!("join-mismatch (seq {call_seq})"),
                            None => format!("no matching tool_use (seq {call_seq})"),
                        };
                        let mut label = format!("tool {tool}");
                        if let Some(n) = note {
                            label.push_str(&format!(" [{n}]"));
                        }
                        traces[i].spans.push(SpanNode {
                            label,
                            detail,
                            duration_ms: Some(*duration_ms),
                            usage: None,
                            ok,
                        });
                    }
                }
                // Compacted: a view-rewriting event — the trace spans are built
                // from what actually ran, so nothing to record here.
                ObsBody::Compacted { .. } => {}
            },
            SessionEntry::Message { message, .. } => {
                // Collect tool_use blocks for the join; summarize other blocks
                // only when there is no obs stream at all (v1 fallback outline).
                let mut tool_uses: Vec<PendingToolUse> = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::ToolUse { name, input, .. } => {
                            tool_uses.push(PendingToolUse {
                                name: name.clone(),
                                digest: crab_core::obs::digest(input),
                                summary: crate::width::truncate_to_width(
                                    &crab_core::util::canonical_json(input),
                                    60,
                                ),
                            });
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            // Only as v1 outline material: if a trace is open the
                            // result is visible through the tool_call span already.
                            if current.is_none() {
                                let mut label = format!("result {tool_use_id}");
                                if *is_error {
                                    label.push_str(" [error]");
                                }
                                push_outline(
                                    &mut traces,
                                    SpanNode {
                                        label,
                                        detail: crate::width::truncate_to_width(content, 60),
                                        duration_ms: None,
                                        usage: None,
                                        ok: !is_error,
                                    },
                                );
                            }
                        }
                        ContentBlock::Text { text } => {
                            if current.is_none() {
                                let role = match message.role {
                                    crab_core::types::Role::User => "user",
                                    crab_core::types::Role::Assistant => "assistant",
                                };
                                push_outline(
                                    &mut traces,
                                    SpanNode {
                                        label: role.to_string(),
                                        detail: crate::width::truncate_to_width(text, 60),
                                        duration_ms: None,
                                        usage: None,
                                        ok: true,
                                    },
                                );
                            }
                        }
                    }
                }
                if current.is_some() {
                    pending.extend(tool_uses);
                }
            }
            SessionEntry::Meta { .. } => {}
        }
    }
    traces
}

/// v1 files have no obs: one pseudo-trace that just outlines the conversation.
fn push_outline(traces: &mut Vec<Trace>, span: SpanNode) {
    if traces.is_empty() {
        traces.push(Trace {
            run_id: 0,
            spans: Vec::new(),
            complete: false,
            start_ts: 0,
        });
    }
    traces[0].spans.push(span);
}

fn outcome_parts(outcome: &ObsOutcome) -> (bool, Option<String>) {
    match outcome {
        ObsOutcome::Ok => (true, None),
        ObsOutcome::Aborted => (false, Some("aborted".to_string())),
        ObsOutcome::Error(e) => (false, Some(format!("error: {}", crate::width::truncate_to_width(e, 40)))),
    }
}

fn kfmt(n: u32) -> String {
    if n >= 1000 {
        format!("{}.{:01}k", n / 1000, (n % 1000) / 100)
    } else {
        n.to_string()
    }
}

/// Text-tree render of the traces.
pub fn render(traces: &[Trace]) -> String {
    let mut out = String::new();
    for t in traces {
        let status = if t.complete { "" } else { " · INCOMPLETE" };
        out.push_str(&format!("run {} · {} spans{}\n", t.run_id, t.spans.len(), status));
        for (i, s) in t.spans.iter().enumerate() {
            let last = i + 1 == t.spans.len();
            let branch = if last { "└──" } else { "├──" };
            let mark = if s.ok { "✓" } else { "✗" };
            let mut line = format!("{branch} {mark} {}", s.label);
            if let Some(d) = s.duration_ms {
                line.push_str(&format!(" · {}ms", d));
            }
            if let Some(u) = s.usage {
                line.push_str(&format!(
                    " · ↑{} ↓{} (cr{} cw{})",
                    kfmt(u.completion),
                    kfmt(u.prompt),
                    kfmt(u.cache_read),
                    kfmt(u.cache_write)
                ));
            }
            if !s.detail.is_empty() {
                line.push_str(&format!(" │ {}", s.detail));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    if out.is_empty() {
        out.push_str("(no traceable events)\n");
    }
    out
}

pub fn run(file: &Path, json: bool) -> anyhow::Result<()> {
    let session = Session::load(file)?;
    let traces = build_traces(session.entries());
    if json {
        println!("{}", serde_json::to_string_pretty(&traces)?);
    } else {
        print!("{}", render(&traces));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_core::obs::{ObsEvent, ObsOutcome};
    use crab_core::types::{Message, Role};

    fn tool_use_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "let me read".into() },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                },
            ],
        }
    }

    fn obs(id: u64, body: ObsBody) -> SessionEntry {
        SessionEntry::Obs {
            id,
            event: ObsEvent { run_id: 0, ts: 1000 + id, body },
        }
    }

    fn msg_entry(id: u64, m: Message) -> SessionEntry {
        SessionEntry::Message {
            id,
            parent_id: None,
            message: m,
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn v2_run_builds_spans_and_joins_tool_args() {
        let entries = vec![
            SessionEntry::Meta { format_version: 2, created: "x".into() },
            // Real ordering: the agent emits TurnStart before recording the user message.
            obs(2, ObsBody::TurnStart),
            msg_entry(1, text_msg(Role::User, "hi")),
            msg_entry(3, tool_use_msg()),
            obs(4, ObsBody::ToolCall {
                tool: "read".into(),
                call_seq: 0,
                args_digest: crab_core::obs::digest(&serde_json::json!({"path": "a.rs"})),
                result_digest: crab_core::obs::digest(&serde_json::Value::String("file body".into())),
                bytes_in: 15,
                bytes_out: 9,
                duration_ms: 3,
                outcome: ObsOutcome::Ok,
            }),
            obs(5, ObsBody::LlmCall {
                provider: "mock".into(),
                model: "mock".into(),
                duration_ms: 9,
                usage: Some(LlmUsage { prompt: 100, completion: 10, cache_read: 0, cache_write: 0 }),
                outcome: ObsOutcome::Ok,
            }),
            obs(6, ObsBody::TurnEnd {
                stop_reason: crab_core::types::StopReason::EndTurn,
                interrupted: false,
                llm_calls: 1,
                tool_calls: 1,
            }),
        ];
        let traces = build_traces(&entries);
        assert_eq!(traces.len(), 1);
        let t = &traces[0];
        assert!(t.complete);
        assert_eq!(t.spans.len(), 2);
        assert_eq!(t.spans[0].label, "tool read");
        assert!(t.spans[0].detail.contains("read"), "got {}", t.spans[0].detail);
        assert!(t.spans[0].detail.contains("\"path\":\"a.rs\""), "got {}", t.spans[0].detail);
        assert_eq!(t.spans[1].usage.unwrap().prompt, 100);
    }

    #[test]
    fn digest_mismatch_marks_join_mismatch() {
        let entries = vec![
            obs(0, ObsBody::TurnStart),
            msg_entry(1, tool_use_msg()),
            obs(2, ObsBody::ToolCall {
                tool: "read".into(),
                call_seq: 0,
                args_digest: "deadbeef".into(),
                result_digest: "x".into(),
                bytes_in: 0,
                bytes_out: 0,
                duration_ms: 1,
                outcome: ObsOutcome::Ok,
            }),
            obs(3, ObsBody::TurnEnd {
                stop_reason: crab_core::types::StopReason::EndTurn,
                interrupted: false,
                llm_calls: 0,
                tool_calls: 1,
            }),
        ];
        let traces = build_traces(&entries);
        assert!(traces[0].spans[0].detail.contains("join-mismatch"), "{}", traces[0].spans[0].detail);
    }

    #[test]
    fn missing_turn_end_marks_incomplete() {
        let entries = vec![obs(0, ObsBody::TurnStart), obs(1, ObsBody::TurnStart)];
        let traces = build_traces(&entries);
        assert_eq!(traces.len(), 2);
        assert!(!traces[0].complete);
        assert!(!traces[1].complete);
    }

    #[test]
    fn v1_messages_render_as_outline() {
        let entries = vec![
            msg_entry(0, text_msg(Role::User, "hello")),
            msg_entry(1, tool_use_msg()),
        ];
        let traces = build_traces(&entries);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].spans.iter().any(|s| s.label == "user"));
        assert!(render(&traces).contains("user"));
    }

    #[test]
    fn render_marks_errors() {
        let entries = vec![
            obs(0, ObsBody::TurnStart),
            obs(1, ObsBody::ToolCall {
                tool: "bash".into(),
                call_seq: 0,
                args_digest: "x".into(),
                result_digest: "x".into(),
                bytes_in: 0,
                bytes_out: 0,
                duration_ms: 1,
                outcome: ObsOutcome::Error("boom".into()),
            }),
            obs(2, ObsBody::TurnEnd {
                stop_reason: crab_core::types::StopReason::EndTurn,
                interrupted: false,
                llm_calls: 0,
                tool_calls: 1,
            }),
        ];
        let out = render(&build_traces(&entries));
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("error: boom"), "{out}");
    }
}
