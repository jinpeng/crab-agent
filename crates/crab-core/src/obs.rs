//! Observability events (obs) — measurement-only facts appended to the session
//! log. NEVER part of the model-visible history (spec: 日志即真相).

use serde::{Deserialize, Serialize};

/// Usage reported by the provider, four-way split (DSH tokenUsage shape).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ObsOutcome {
    Ok,
    /// Truncated to 128 chars; full detail lives in the message log.
    Error(String),
    Aborted,
}

/// One observability record. `run_id` identifies one `Agent::run` call
/// (one user message = one trace).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObsEvent {
    pub run_id: u64,
    pub ts: u64, // unix epoch millis
    #[serde(flatten)]
    pub body: ObsBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "obs", rename_all = "snake_case")]
pub enum ObsBody {
    TurnStart,
    TurnEnd {
        stop_reason: crate::types::StopReason,
        interrupted: bool,
        llm_calls: u32,
        tool_calls: u32,
    },
    LlmCall {
        provider: String,
        model: String,
        duration_ms: u64,
        usage: Option<LlmUsage>,
        outcome: ObsOutcome,
    },
    ToolCall {
        tool: String,
        call_seq: u32,
        args_digest: String,
        result_digest: String,
        bytes_in: u64,
        bytes_out: u64,
        duration_ms: u64,
        outcome: ObsOutcome,
    },
}

/// Canonical-json digest (blake3, first 8 hex chars): object keys are sorted
/// so logically-equal args hash equal regardless of map order.
pub fn digest(v: &serde_json::Value) -> String {
    let canonical = crate::util::canonical_json(v);
    let mut h = blake3::Hasher::new();
    h.update(canonical.as_bytes());
    h.finalize().to_hex()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_roundtrips_with_cache_fields() {
        let u = LlmUsage {
            prompt: 10,
            completion: 5,
            cache_read: 3,
            cache_write: 7,
        };
        let s = serde_json::to_string(&u).unwrap();
        assert_eq!(serde_json::from_str::<LlmUsage>(&s).unwrap(), u);
    }

    #[test]
    fn obs_event_uses_obs_tag() {
        let e = ObsEvent {
            run_id: 0,
            ts: 1,
            body: ObsBody::TurnStart,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["obs"], "turn_start");
    }

    #[test]
    fn digest_is_key_order_independent() {
        let a = serde_json::json!({"path": "x", "limit": 1});
        let b = serde_json::json!({"limit": 1, "path": "x"});
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&serde_json::json!({"path": "y", "limit": 1})));
        assert_eq!(digest(&a).len(), 8);
    }
}
