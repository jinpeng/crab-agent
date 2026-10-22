//! ContextMeter — deterministic context occupancy by replaying the session
//! log (dsh token-meter idea, crab's minimal landing: replay 日志做确定性测量).
//!
//! Pure read of what the provider already reported — no model calls, no
//! token estimation (DESIGN.md Q16: provider usage is the only exact anchor).
//! 测量不改变系统： this module never writes; the log stays the sole 事实源.

use crate::obs::ObsBody;
use crate::session::SessionEntry;

/// Occupancy of the context window: the prompt-token count of the most recent
/// LLM call that carried usage. `prompt` includes cache reads (the
/// DeepSeek/Kimi convention), so this is real occupancy, not fresh input.
///
/// Returns `None` when the log holds no usage yet — a fresh session, a v1
/// session written before obs events existed, or a session whose calls all
/// lacked usage. Callers must render "unknown" rather than fabricate a number
/// (the same honesty rule as unpriced models showing '-' instead of $0).
pub fn context_used(entries: &[SessionEntry]) -> Option<u64> {
    entries.iter().rev().find_map(|e| match e {
        SessionEntry::Obs {
            event:
                crate::obs::ObsEvent {
                    body: ObsBody::LlmCall {
                        usage: Some(u), ..
                    },
                    ..
                },
            ..
        } => Some(u.prompt as u64),
        _ => None,
    })
}

/// Prompt-token count of every LlmCall that carried usage, in log order.
/// `compact::plan` derives per-pair sizes from consecutive differences of
/// this sequence (spec §3 fold→尺寸映射).
pub fn prompt_sizes(entries: &[SessionEntry]) -> Vec<u64> {
    entries
        .iter()
        .filter_map(|e| match e {
            SessionEntry::Obs {
                event:
                    crate::obs::ObsEvent {
                        body: ObsBody::LlmCall {
                            usage: Some(u), ..
                        },
                        ..
                    },
                ..
            } => Some(u.prompt as u64),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompt_sizes_collects_usage_carrying_calls_in_order() {
        let entries = vec![
            llm_call(Some(usage(10_000))),
            llm_call(None),
            llm_call(Some(usage(14_500))),
        ];
        assert_eq!(prompt_sizes(&entries), vec![10_000, 14_500]);
    }

    #[test]
    fn prompt_sizes_empty_when_no_usage() {
        assert!(prompt_sizes(&[msg(Role::User, "hi")]).is_empty());
    }
    use super::*;
    use crate::obs::{ObsEvent, ObsOutcome};
    use crate::session::{Session, SessionEntry};
    use crate::types::{ContentBlock, Message, Role};

    fn meta() -> SessionEntry {
        SessionEntry::Meta {
            format_version: 2,
            created: "2026-10-21".into(),
        }
    }

    fn msg(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message {
            id: 1,
            parent_id: None,
            message: Message {
                role,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
        }
    }

    fn llm_call(usage: Option<crate::obs::LlmUsage>) -> SessionEntry {
        SessionEntry::Obs {
            id: 2,
            event: ObsEvent {
                run_id: 0,
                ts: 1,
                body: ObsBody::LlmCall {
                    provider: "mock".into(),
                    model: "mock".into(),
                    duration_ms: 5,
                    usage,
                    outcome: ObsOutcome::Ok,
                },
            },
        }
    }

    fn usage(prompt: u32) -> crate::obs::LlmUsage {
        crate::obs::LlmUsage {
            prompt,
            ..Default::default()
        }
    }

    #[test]
    fn returns_prompt_of_latest_call_with_usage() {
        let entries = vec![
            meta(),
            msg(Role::User, "hi"),
            llm_call(Some(usage(12_000))),
            llm_call(Some(usage(18_500))),
        ];
        assert_eq!(context_used(&entries), Some(18_500));
    }

    #[test]
    fn call_without_usage_is_skipped_in_favour_of_earlier_one() {
        let entries = vec![
            llm_call(Some(usage(9_000))),
            llm_call(None), // mock provider / aborted before headers
        ];
        assert_eq!(context_used(&entries), Some(9_000));
    }

    #[test]
    fn all_calls_without_usage_is_none() {
        let entries = vec![msg(Role::User, "hi"), llm_call(None)];
        assert_eq!(context_used(&entries), None);
    }

    #[test]
    fn v1_shape_messages_only_is_none() {
        // A v1 session: no meta row, no obs events — nothing to measure.
        let entries = vec![msg(Role::User, "hi")];
        assert_eq!(context_used(&entries), None);
    }

    #[test]
    fn empty_log_is_none() {
        assert_eq!(context_used(&[]), None);
    }

    #[test]
    fn message_rows_after_last_call_do_not_move_the_fold() {
        // Tool results / assistant text appended after the final call are not
        // in any prompt yet — the last provider-reported number stands.
        let entries = vec![
            llm_call(Some(usage(7_000))),
            msg(Role::Assistant, "done"),
        ];
        assert_eq!(context_used(&entries), Some(7_000));
    }

    #[test]
    fn roundtrips_through_a_real_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        })
        .unwrap();
        s.append_obs(ObsEvent {
            run_id: 0,
            ts: 1,
            body: ObsBody::LlmCall {
                provider: "mock".into(),
                model: "mock".into(),
                duration_ms: 5,
                usage: Some(usage(42_000)),
                outcome: ObsOutcome::Ok,
            },
        })
        .unwrap();
        let loaded = Session::load(s.path()).unwrap();
        assert_eq!(context_used(loaded.entries()), Some(42_000));
    }
}
