//! Compaction: 确定性瘦身 + 按需摘要（spec 2026-10-21）。纯函数模块——
//! plan 决策、view 变换都不碰 IO、不碰模型；agent loop 是薄适配器。

use crate::obs::CompactionStrategy;
use crate::types::{ContentBlock, Message, ToolCallId};
use std::collections::HashMap;

/// Text 块超过此长度进入瘦身（spec §5）。
const SHED_TEXT_CHARS: usize = 2048;
const SHED_HEAD_CHARS: usize = 200;

/// 确定性瘦身单条消息（spec §5）。不改 role、不删块——wire 配对完整。
pub fn shed_message(m: &Message, tool_names: &HashMap<ToolCallId, String>) -> Message {
    let content = m
        .content
        .iter()
        .map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                let name = tool_names
                    .get(tool_use_id)
                    .map(String::as_str)
                    .unwrap_or("tool result");
                ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: format!(
                        "[shed: {name} → {} bytes; re-run the tool if needed]",
                        content.len()
                    ),
                    is_error: *is_error,
                }
            }
            ContentBlock::Text { text } if text.chars().count() > SHED_TEXT_CHARS => {
                let head: String = text.chars().take(SHED_HEAD_CHARS).collect();
                ContentBlock::Text {
                    text: format!("{head}…({} chars total, shed)", text.chars().count()),
                }
            }
            other => other.clone(),
        })
        .collect();
    Message { role: m.role, content }
}

/// 压缩预算（spec §3）。
pub struct Budget {
    /// 触发压缩的占用比（config auto_compact_threshold；plan 不消费，
    /// 由 agent 的 compaction_due 判定）。
    pub trigger: f64,
    /// 保留后缀的累计尺寸上限，占 window 比例（0.4）。
    pub keep: f64,
    /// context window；None → 字节预算近似（spec §7）。
    pub window: Option<u64>,
}

/// 压缩计划（spec §3）。
#[derive(Debug, PartialEq)]
pub struct Plan {
    /// history[keep_from..] 原样保留；0 = 无意义不压。
    pub keep_from: usize,
    /// 瘦身后仍超预算 → 需要摘要路径（§6）。
    pub need_summary: bool,
}

/// 已应用的压缩（内存态）。`keep_from` 是绝对消息序号——append-only
/// 日志上消息序号稳定，resume 重放时同一序号成立。
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedCompaction {
    pub keep_from: usize,
    pub strategy: CompactionStrategy,
    pub summary: Option<String>,
}

fn text_bytes(msgs: &[Message]) -> usize {
    msgs.iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            crate::types::ContentBlock::Text { text } => text.len(),
            crate::types::ContentBlock::ToolUse { input, .. } => {
                crate::util::canonical_json(input).len()
            }
            crate::types::ContentBlock::ToolResult { content, .. } => content.len(),
        })
        .sum()
}

pub fn plan(history: &[Message], sizes: &[u64], budget: &Budget) -> Plan {
    let pairs = history.len() / 2;
    if pairs <= 2 {
        return Plan { keep_from: 0, need_summary: false };
    }
    // pair k 的估计尺寸：s_k = sizes[k+1] − sizes[k]；末 pair 重复前一 s。
    let pair_size = |k: usize| -> u64 {
        if k + 1 < sizes.len() {
            sizes[k + 1].saturating_sub(sizes[k])
        } else if sizes.len() >= 2 {
            sizes[sizes.len() - 1].saturating_sub(sizes[sizes.len() - 2])
        } else {
            sizes.first().copied().unwrap_or(0)
        }
    };
    let target = match budget.window {
        Some(w) => (budget.keep * w as f64) as u64,
        None => (budget.keep * text_bytes(history) as f64) as u64,
    };
    // 从尾部数：保留不超过 target 的最大 pair 数（≥2）
    let mut m = 2usize;
    let mut acc = pair_size(pairs - 1) + pair_size(pairs - 2);
    while m < pairs && acc + pair_size(pairs - m - 1) <= target {
        acc += pair_size(pairs - m - 1);
        m += 1;
    }
    // need_summary：瘦身后 prefix 估计仍超 target。估计法：瘦身变换后的
    // prefix 字节数 × 全局 tokens/byte 比率 r（Q16 允许的事前决策估算，
    // 仅供决策、不展示）。r 分母为 0（无内容）→ 不摘要。
    let prefix = &history[..2 * (pairs - m)];
    let r = match (sizes.last(), text_bytes(history)) {
        (Some(last), tb) if tb > 0 => *last as f64 / tb as f64,
        _ => 0.0,
    };
    let shed_prefix_bytes: usize = prefix
        .iter()
        .flat_map(|msg| &msg.content)
        .map(|b| match b {
            crate::types::ContentBlock::ToolResult { .. } => 48, // 占位符近似
            crate::types::ContentBlock::Text { text } => text.len().min(2048),
            crate::types::ContentBlock::ToolUse { .. } => 64,
        })
        .sum();
    let need_summary = (shed_prefix_bytes as f64 * r) as u64 > target && !prefix.is_empty();
    Plan { keep_from: 2 * (pairs - m), need_summary }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, Role};

    fn user(text: &str) -> Message {
        Message { role: Role::User, content: vec![ContentBlock::Text { text: text.into() }] }
    }
    fn assistant(text: &str) -> Message {
        Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: text.into() }] }
    }
    fn assistant_tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/x"}),
            }],
        }
    }
    fn user_tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }
    /// n 个 (user, assistant) 对
    fn history(n: usize) -> Vec<Message> {
        (0..n).flat_map(|i| [user(&format!("u{i}")), assistant(&format!("a{i}"))]).collect()
    }
    /// 单调递增的 prompt 序列（每次调用 +1000 tokens）
    fn sizes(n: usize) -> Vec<u64> {
        (0..n).map(|i| (i as u64 + 1) * 1000).collect()
    }

    fn budget(window: u64) -> Budget {
        Budget { trigger: 0.8, keep: 0.4, window: Some(window) }
    }

    use crate::types::ToolCallId;
    use std::collections::HashMap;

    #[test]
    fn sheds_tool_result_content_but_keeps_the_block() {
        let mut names = HashMap::new();
        names.insert("c1".to_string(), "read".to_string());
        let m = user_tool_result("c1", &"x".repeat(12_400));
        let shed = shed_message(&m, &names);
        let ContentBlock::ToolResult { content, is_error, tool_use_id } = &shed.content[0] else {
            panic!("block kind changed");
        };
        assert_eq!(tool_use_id, "c1");
        assert!(!is_error);
        assert!(content.contains("[shed: read"), "{content}");
        assert!(content.contains("12400 bytes"), "{content}");
        assert!(!content.starts_with('x'));
    }

    #[test]
    fn truncates_long_text_blocks() {
        let m = user(&"word ".repeat(600)); // 3000 chars
        let shed = shed_message(&m, &HashMap::new());
        let ContentBlock::Text { text } = &shed.content[0] else {
            panic!("block kind changed");
        };
        assert!(text.contains("chars total, shed"), "{text}");
        assert!(text.chars().count() < 400, "{}", text.chars().count());
    }

    #[test]
    fn short_text_and_tooluse_args_pass_through() {
        let m = user("short question");
        assert_eq!(shed_message(&m, &HashMap::new()), m);
        let m = assistant_tool_use("c1");
        assert_eq!(shed_message(&m, &HashMap::new()), m);
    }

    #[test]
    fn keeps_largest_suffix_under_budget_min_two_pairs() {
        // 8 pairs, each ~1000 tok; target 40% of 6000 = 2400 → 2 pairs
        let h = history(8);
        let p = plan(&h, &sizes(8), &budget(6000));
        assert_eq!(p.keep_from, 2 * (8 - 2));
    }

    #[test]
    fn no_summary_when_prefix_sheds_to_nothing() {
        // prefix 主体是一个大 ToolResult（可 shed）；shed 后 prefix 极小 → 无需摘要
        let big = "x".repeat(8000);
        let h = vec![
            user("q"),
            assistant_tool_use("c1"),
            user_tool_result("c1", &big),
            assistant("ok"),
            user("q2"),
            assistant("done2"),
            user("q3"),
            assistant("done3"),
        ];
        let sizes = vec![1000, 5000, 9000, 13_000]; // 增长主要来自 tool_result
        let p = plan(&h, &sizes, &budget(16_000)); // target 6400 < 8000 → m=2
        assert_eq!(p.keep_from, 4);
        assert!(!p.need_summary, "shed prefix is tiny; no summary needed");
    }

    #[test]
    fn keeps_more_pairs_when_window_allows() {
        // target 40% of 20000 = 8000 → 8 pairs 全保 → keep_from=0 → 无事可压
        let h = history(8);
        let p = plan(&h, &sizes(8), &budget(20_000));
        assert_eq!(p.keep_from, 0);
    }

    #[test]
    fn nothing_to_compact_when_two_or_fewer_pairs() {
        let h = history(2);
        let p = plan(&h, &sizes(2), &budget(100));
        assert_eq!(p.keep_from, 0);
        assert!(!p.need_summary);
    }

    #[test]
    fn odd_trailing_user_message_is_always_kept() {
        // 5 对 + 1 条落单 user；keep_from 必须落在 user 边界（偶数索引）
        let mut h = history(5);
        h.push(user("current input"));
        let p = plan(&h, &sizes(5), &budget(3000)); // target 1200 → 抬到最小 2 对
        assert_eq!(p.keep_from, 2 * (5 - 2));
        assert!(p.keep_from % 2 == 0);
    }

    #[test]
    fn requests_summary_when_shedding_cannot_reach_budget() {
        // 窗口极小：target 400 tok，2 对也要 2000 tok → 摘要不可避免
        let h = history(6);
        let p = plan(&h, &sizes(6), &budget(1000));
        assert!(p.need_summary);
        assert!(p.keep_from > 0);
    }

    #[test]
    fn byte_fallback_when_window_unknown() {
        let h = history(6); // "u0"/"a0"…每条 ~2-3 字节
        let p = plan(&h, &sizes(6), &Budget { trigger: 0.8, keep: 0.4, window: None });
        // 确定性即可：不 panic、keep_from 落在边界
        assert!(p.keep_from % 2 == 0);
    }
}
