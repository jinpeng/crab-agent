# Compaction 实施计划（混合式：确定性瘦身 + 按需摘要）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `--continue` 恢复和长时间运行时，通过"确定性瘦身 + 按需 LLM 摘要"把请求视图压回预算内，日志保持 append-only 唯一事实源。

**Architecture:** 纯函数模块 `crab-core/src/compact.rs`（`plan` 决策 + `view` 变换）+ obs 事件 `Compacted` + agent loop 在 `request_msgs` seam 应用视图。spec 中的 `MeterFold` 落地为 `meter::prompt_sizes()`（简单函数，不上结构体）。

**Tech Stack:** Rust workspace（crab-core / crab-cli），现有 MockProvider 脚本测试法，tempfile。

**Spec:** `docs/superpowers/specs/2026-10-21-compaction-design.md`

**关键既有事实（执行者零上下文也可靠）：**
- history 恒为 `(user, assistant)` 对交替、以 user 起始；奇数长度时尾条是落单的 user 消息（当前输入），任何水位线必须保住它。
- `Session::append_obs` 同步写盘**并**更新内存 `entries`——`meter` replay 永远读得到最新状态。
- `SessionEntry::id()` 已存在；`Agent::new` 用 `session.messages()` 重建 history（agent.rs:72-76）。
- `MockProvider::new(vec![脚本1, 脚本2, ...])`：**每次 `complete()` pop 一个脚本**（从 Vec 头 pop），脚本为 `Vec<StreamEvent>`；`StreamEvent::Usage{prompt_tokens,..}` 产生带 usage 的 turn。
- `AgentConfig` 现有 `system_prompt` 一个字段，`AgentConfig::default()` 被测试大量使用——新增字段必须保持 Default 语义安全。
- TUI 斜杠命令链路：`parse_slash`（state.rs:242）→ `SlashCommand` → `UiCommand`（mod.rs:260）→ select loop（mod.rs:163-208）→ `state.notice(...)` 打通知。
- 运行中路径零改动原则：`AgentEvent::Usage` → `status.add_usage` 照旧。

---

## 文件结构

| 文件 | 职责 | 动作 |
|---|---|---|
| `crates/crab-core/src/obs.rs` | `CompactionStrategy` + `ObsBody::Compacted` | Modify |
| `crates/crab-core/src/meter.rs` | `prompt_sizes()` per-call prompt 序列 | Modify |
| `crates/crab-core/src/compact.rs` | `Budget`/`Plan`/`AppliedCompaction` + `plan`/`shed_message`/`view`/`resolve` | Create |
| `crates/crab-core/src/agent.rs` | `AgentConfig` 扩字段、`compactions` 状态、`run_compaction`、自动触发、`AgentEvent::Compacted` | Modify |
| `crates/crab-core/src/config.rs` | `auto_compact_threshold` 配置 + env + 默认值解析 | Modify |
| `crates/crab-cli/src/app.rs` | `build_agent` 接线（threshold/context_window/compactions resolve） | Modify |
| `crates/crab-cli/src/tui/state.rs` | `/compact` 解析 | Modify |
| `crates/crab-cli/src/tui/mod.rs` | `UiCommand::Compact` 分发 + `AgentEvent::Compacted` 通知 | Modify |
| `README.md` | config 字段 + slash 表 + compaction 简介 | Modify |

---

## Task 1: obs 事件类型

**Files:**
- Modify: `crates/crab-core/src/obs.rs`（`ObsBody` 枚举 + 新类型）
- Test: `crates/crab-core/src/obs.rs` tests 模块

- [ ] **Step 1: 写失败测试**（obs.rs 的 `mod tests` 内追加）

```rust
    #[test]
    fn compacted_event_roundtrips() {
        let e = ObsEvent {
            run_id: 3,
            ts: 42,
            body: ObsBody::Compacted {
                up_to: 17,
                strategy: CompactionStrategy::Summarize,
                shed_tool_results: 12,
                turns_affected: 6,
                summary: Some("we decided X".into()),
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["obs"], "compacted");
        assert_eq!(v["up_to"], 17);
        assert_eq!(v["strategy"], "summarize");
        assert_eq!(serde_json::from_str::<ObsEvent>(&s).unwrap(), e);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core obs::tests::compacted_event_roundtrips`
Expected: FAIL（`CompactionStrategy` 未定义）

- [ ] **Step 3: 实现**（obs.rs，`ObsBody` 枚举前加类型、枚举中加变体）

```rust
/// How a compaction condensed the prefix (spec §2).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// Deterministic: tool results shed, text truncated (spec §5).
    Shed,
    /// Prefix replaced by an LLM-written summary (spec §6).
    Summarize,
}
```

`ObsBody` 中加（注意：现有变体带 `#[serde(tag = "obs", ...)]`，新变体同属一族）：

```rust
    /// One compaction pass. `up_to` is the EntryId of the last message folded
    /// into the compressed view; the durable log itself is never modified
    /// (日志即真相，视图可重建).
    Compacted {
        up_to: crate::types::EntryId,
        strategy: CompactionStrategy,
        shed_tool_results: u32,
        turns_affected: u32,
        /// `Some` iff strategy == Summarize. Inline, 单文件原则.
        summary: Option<String>,
    },
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core obs`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): Compacted obs event — the replayable compaction record`

---

## Task 2: meter::prompt_sizes

**Files:**
- Modify: `crates/crab-core/src/meter.rs`

- [ ] **Step 1: 写失败测试**（meter.rs tests 追加）

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core meter`
Expected: FAIL（`prompt_sizes` 未定义）

- [ ] **Step 3: 实现**（meter.rs，紧跟 `context_used` 之后）

```rust
/// Prompt-token count of every LlmCall that carried usage, in log order.
/// `plan` derives per-pair sizes from consecutive differences of this
/// sequence (spec §3 fold→尺寸映射).
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core meter`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): meter::prompt_sizes — per-call prompt sequence for compaction planning`

---

## Task 3: compact::plan

**Files:**
- Create: `crates/crab-core/src/compact.rs`
- Modify: `crates/crab-core/src/lib.rs`（加 `pub mod compact;`）

**plan 算法（确定性，无 tokenizer）：**
- `sizes` = prompt_sizes 结果；pair k 的估计尺寸 `s_k = sizes[k+1] − sizes[k]`（最后一个 pair 重复前一个 `s`，只有一个 size 时用该值）。
- `target = keep × window`（window 为 None 时退化为字节：`target = keep × history 全部文本字节数`）。
- 从尾部保留**不超过 target 的最大** pair 数 `m`（至少 2）；`keep_from = 2×(pairs − m)`（奇数历史长度的落单尾消息自动在内）。
- pairs ≤ 2 → `keep_from = 0`（无意义不压）。
- `need_summary`：瘦身后的 prefix 估计仍 > target。估计法：prefix 瘦身变换后的字节数 × 全局 tokens/byte 比率 `r = sizes.last() / history 总字节数`（下限 r=0 时不摘要——分母为 0 说明没内容可压）。这是 Q16 允许的"事前决策估算"，仅供决策不展示。

- [ ] **Step 1: 写失败测试**（compact.rs tests）

```rust
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

    #[test]
    fn keeps_largest_suffix_under_budget_min_two_pairs() {
        // 8 pairs, each ~1000 tok; target 40% of 6000 = 2400 → 2 pairs
        let h = history(8);
        let p = plan(&h, &sizes(8), &budget(6000));
        assert_eq!(p.keep_from, 2 * (8 - 2));
        assert!(!p.need_summary);
    }

    #[test]
    fn keeps_more_pairs_when_window_allows() {
        // target 40% of 20000 = 8000 → 8 pairs 全保（但只有 8 次调用 7 个 delta，
        // 第 8 对重复 s_6=1000）→ m=8，keep_from=0 → 无事可压
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
        let p = plan(&h, &sizes(5), &budget(3000)); // target 1200 → 1 对→抬到 2 对
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core compact`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**（compact.rs 首部；lib.rs 加 `pub mod compact;`，插在 `pub mod config;` 后）

```rust
//! Compaction: 确定性瘦身 + 按需摘要（spec 2026-10-21）。纯函数模块——
//! plan 决策、view 变换都不碰 IO、不碰模型；agent loop 是薄适配器。

use crate::obs::CompactionStrategy;
use crate::types::Message;

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
    // need_summary：瘦身后 prefix 估计仍超 target
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core compact`
Expected: 全部 PASS（若 `keeps_more_pairs_when_window_allows` 因 off-by-one 失败：m 上限取 `pairs`，`acc` 初值为末两 pair，逐对向上扩，核对边界后修实现或测试期望）

- [ ] **Step 5: Commit** `feat(core): compact::plan — deterministic watermark under budget`

---

## Task 4: compact::shed_message

**Files:**
- Modify: `crates/crab-core/src/compact.rs`

**规则（spec §5）**：`ToolResult` 正文 → `[shed: {tool_name} → {n} bytes; re-run the tool if needed]`（tool_name 从 prefix 内配对 ToolUse 的 id→name 表查，查不到用 `"tool result"`）；`Text` 块 >2048 chars → 前 200 chars + `…({n} chars total, shed)`；其余原样。

- [ ] **Step 1: 写失败测试**（compact.rs tests 追加）

```rust
    use crate::types::ToolCallId;
    use std::collections::HashMap;

    fn tool_result_msg(id: ToolCallId, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: content.into(),
                is_error: false,
            }],
        }
    }

    fn tool_use_msg(name: &str, id: ToolCallId) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id,
                name: name.into(),
                input: serde_json::json!({"path": "/x/y.rs"}),
            }],
        }
    }

    #[test]
    fn sheds_tool_result_content_but_keeps_the_block() {
        let mut names = HashMap::new();
        names.insert("c1".to_string(), "read".to_string());
        let m = tool_result_msg("c1", &"x".repeat(12_400));
        let shed = shed_message(&m, &names);
        let ContentBlock::ToolResult { content, is_error, tool_use_id } = &shed.content[0] else {
            panic!("block kind changed");
        };
        assert_eq!(tool_use_id, "c1");
        assert!(!is_error);
        assert!(content.contains("[shed: read"), "{content}");
        assert!(content.contains("12,400 bytes") || content.contains("12400 bytes"), "{content}");
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
        let m = tool_use_msg("read", "c1");
        assert_eq!(shed_message(&m, &HashMap::new()), m);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core compact::tests::sheds`
Expected: FAIL（`shed_message` 未定义）

- [ ] **Step 3: 实现**（compact.rs）

```rust
use std::collections::HashMap;
use crate::types::{ContentBlock, ToolCallId};

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
                let name = tool_names.get(tool_use_id).map(String::as_str).unwrap_or("tool result");
                ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: format!("[shed: {name} → {} bytes; re-run the tool if needed]", content.len()),
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core compact`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): shed_message — deterministic tool-result shedding`

---

## Task 5: compact::view + resolve

**Files:**
- Modify: `crates/crab-core/src/compact.rs`

**语义**：`view` 只认**最后一条** `AppliedCompaction`（水位线单调递增，last-wins 即全量 fold）；`keep_from` 必须偶数（user 边界），`debug_assert`。Summarize：prefix → 一条 user 消息（`[Earlier conversation summary]` + summary）。`resolve` 把日志里的 `Compacted{up_to: EntryId}` 映射为 `AppliedCompaction{keep_from: 消息序号}`（≤up_to 的 Message 条数）。

- [ ] **Step 1: 写失败测试**（compact.rs tests 追加）

```rust
    use crate::obs::CompactionStrategy;
    use crate::session::{SessionEntry, Session};
    use crate::obs::{ObsEvent, ObsBody};

    fn alternating_assert(view: &[Message]) {
        // wire 不变式：user 起始、严格交替（任何 pair 边界可切）
        for (i, m) in view.iter().enumerate() {
            let expect_user = i % 2 == 0;
            assert_eq!(m.role == Role::User, expect_user, "msg {i} breaks alternation");
        }
        // 每个 tool_use 在紧随的 user 消息里有同 id 的 tool_result
        for (i, m) in view.iter().enumerate() {
            for b in &m.content {
                if let ContentBlock::ToolUse { id, .. } = b {
                    let next = &view[i + 1];
                    assert!(next.content.iter().any(|r| matches!(r, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)));
                }
            }
        }
    }

    fn sample_history() -> Vec<Message> {
        // 3 对：第 0 对有 tool_use/tool_result 配对
        vec![
            user("please read the file"),
            tool_use_msg("read", "c1"),
            tool_result_msg("c1", &"data".repeat(5000)),
            assistant("done reading"),
            user("now summarize"),
            assistant("summary here"),
        ]
    }

    #[test]
    fn empty_compactions_passthrough() {
        let h = sample_history();
        assert_eq!(view(&h, &[]), h);
    }

    #[test]
    fn shed_view_keeps_suffix_verbatim_and_sheds_prefix() {
        let h = sample_history();
        let applied = vec![AppliedCompaction {
            keep_from: 2,
            strategy: CompactionStrategy::Shed,
            summary: None,
        }];
        let v = view(&h, &applied);
        assert_eq!(v.len(), h.len()); // 块不删，只换内容
        alternating_assert(&v);
        // prefix 的 tool_result 被 shed
        let ContentBlock::ToolResult { content, .. } = &v[2].content[0] else {
            panic!()
        };
        assert!(content.contains("[shed:"), "{content}");
        // suffix 逐字节原样
        assert_eq!(v[3..], h[3..]);
    }

    #[test]
    fn summarize_view_replaces_prefix_with_single_user_message() {
        let h = sample_history();
        let v = view(&h, &[AppliedCompaction {
            keep_from: 2,
            strategy: CompactionStrategy::Summarize,
            summary: Some("we read the file".into()),
        }]);
        assert_eq!(v.len(), h.len() - 2 + 1);
        assert!(matches!(&v[0].role, Role::User));
        let ContentBlock::Text { text } = &v[0].content[0] else { panic!() };
        assert!(text.contains("[Earlier conversation summary]"), "{text}");
        assert!(text.contains("we read the file"));
        alternating_assert(&v);
    }

    #[test]
    fn replay_resolve_maps_entry_ids_to_message_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        let id0 = s.append(user("u0")).unwrap();
        s.append(assistant("a0")).unwrap();
        s.append_obs(ObsEvent {
            run_id: 0, ts: 1,
            body: ObsBody::Compacted {
                up_to: id0,
                strategy: CompactionStrategy::Shed,
                shed_tool_results: 0,
                turns_affected: 1,
                summary: None,
            },
        }).unwrap();
        s.append(user("u1")).unwrap();
        s.append(assistant("a1")).unwrap();
        let r = resolve(s.entries());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].keep_from, 1); // ≤ up_to 的 Message 条数
        assert_eq!(r[0].strategy, CompactionStrategy::Shed);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core compact::tests`
Expected: FAIL（`view`/`resolve` 未定义）

- [ ] **Step 3: 实现**（compact.rs）

```rust
use crate::session::SessionEntry;

/// 构造发送给 provider 的 history 视图（spec §3）。
/// 只认最后一条 AppliedCompaction（水位线单调，last-wins）；
/// 空列表 = 全量直通。
pub fn view(history: &[Message], compactions: &[AppliedCompaction]) -> Vec<Message> {
    let Some(last) = compactions.last() else {
        return history.to_vec();
    };
    debug_assert!(last.keep_from % 2 == 0, "watermark must sit on a user boundary");
    let (prefix, suffix) = history.split_at(last.keep_from.min(history.len()));
    match (&last.strategy, &last.summary) {
        (CompactionStrategy::Summarize, Some(summary)) => {
            let mut v = Vec::with_capacity(suffix.len() + 1);
            v.push(Message {
                role: crate::types::Role::User,
                content: vec![
                    ContentBlock::Text { text: "[Earlier conversation summary]".into() },
                    ContentBlock::Text { text: summary.clone() },
                ],
            });
            v.extend_from_slice(suffix);
            v
        }
        _ => {
            // Shed：prefix 逐条确定性瘦身，suffix 原样
            let mut names: HashMap<ToolCallId, String> = HashMap::new();
            for m in prefix {
                for b in &m.content {
                    if let ContentBlock::ToolUse { id, name, .. } = b {
                        names.insert(id.clone(), name.clone());
                    }
                }
            }
            prefix.iter().map(|m| shed_message(m, &names)).chain(suffix.iter().cloned()).collect()
        }
    }
}

/// 从 session entries 重放 Compacted 事件（--continue 路径，spec §2）。
/// up_to 是 Message 条目的 EntryId；keep_from = id ≤ up_to 的 Message 条数。
/// up_to 找不到对应条目的事件跳过（防御坏日志）。
pub fn resolve(entries: &[SessionEntry]) -> Vec<AppliedCompaction> {
    let mut out = Vec::new();
    let mut msg_count = 0usize;
    for e in entries {
        match e {
            SessionEntry::Message { .. } => msg_count += 1,
            SessionEntry::Obs {
                event: crate::obs::ObsEvent {
                    body: crate::obs::ObsBody::Compacted {
                        up_to, strategy, summary, ..
                    },
                    ..
                },
                ..
            } => {
                let hits: usize = entries
                    .iter()
                    .filter(|x| matches!(x, SessionEntry::Message { id, .. } if id <= up_to))
                    .count();
                if hits > 0 {
                    out.push(AppliedCompaction { keep_from: hits, strategy: *strategy, summary: summary.clone() });
                }
                let _ = msg_count; // msg_count 保留给未来审计；当前直接按 id 过滤
            }
            _ => {}
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core compact`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): compact::view + resolve — request view and log replay`

---

## Task 6: agent — AgentConfig 扩字段 + 请求视图接线

**Files:**
- Modify: `crates/crab-core/src/agent.rs`（AgentConfig、Agent 字段、Agent::new、request_msgs 构造）

- [ ] **Step 1: 写失败测试**（agent.rs tests 追加）

```rust
    #[tokio::test]
    async fn summarize_compaction_shrinks_the_request_view_not_the_history() {
        let provider = MockProvider::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::StreamEnd { reason: StopReason::EndTurn },
        ]]);
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut config = AgentConfig::default();
        config.compactions = vec![crate::compact::AppliedCompaction {
            keep_from: 2,
            strategy: crate::obs::CompactionStrategy::Summarize,
            summary: Some("earlier we chose X".into()),
        }];
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        // 预置 3 对历史（直接推入，绕过 run）
        for i in 0..3 {
            agent.history.push(crate::types::Message { role: crate::types::Role::User,
                content: vec![crate::types::ContentBlock::Text { text: format!("u{i}") }] });
            agent.history.push(crate::types::Message { role: crate::types::Role::Assistant,
                content: vec![crate::types::ContentBlock::Text { text: format!("a{i}") }] });
        }
        agent.run("go".into()).await.unwrap();
        let calls = agent.provider_calls();
        let req = calls.last().unwrap();
        // history 保持全量（7 条：3 对 + 当前输入）
        assert_eq!(agent.history().len(), 7);
        // 请求视图缩为 6 条：1 条 summary + 5 条后缀；prefix 原文不在请求里
        assert_eq!(req.len(), 6, "{req:?}");
        assert!(!req.iter().any(|m| m.content.iter().any(|b| matches!(b, crate::types::ContentBlock::Text { text } if text == "u0"))));
        assert!(req[0].content.iter().any(|b| matches!(b, crate::types::ContentBlock::Text { text } if text.contains("[Earlier conversation summary]"))));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core compactions_shrink`
Expected: FAIL（`AgentConfig.compactions` 不存在）

- [ ] **Step 3: 实现**（agent.rs）

`AgentConfig` 扩字段（保持 `Default` 安全：threshold 0.0 = 关闭，Vec/Option 空）：

```rust
#[derive(Debug, Default, Clone)]
pub struct AgentConfig {
    /// Prepended to the provider request as the system message. Empty = not injected.
    /// Not part of session history (it's per-run config, not conversation).
    pub system_prompt: String,
    /// Context window size, when the profile declares one. Drives auto-compaction.
    pub context_window: Option<u64>,
    /// Auto-compact when last prompt exceeds this fraction of the window.
    /// 0.0 (the Default) disables auto-compaction; wired to 0.8 in production.
    pub auto_compact_threshold: f64,
    /// Compactions replayed from the session log at --continue.
    pub compactions: Vec<crate::compact::AppliedCompaction>,
}
```

`Agent` 加字段 + `new` 初始化：

```rust
    compactions: Vec<crate::compact::AppliedCompaction>,
    /// Prompt tokens of the most recent LlmCall (auto-compaction trigger signal).
    last_prompt: Option<u64>,
```

```rust
        Self {
            // ...现有字段...
            compactions: config.compactions.clone(),
            last_prompt: None,
        }
```

`run_inner` 的 `request_msgs` 构造改为走视图（两处 `self.history.clone()` → `view_msgs`）：

```rust
            let view_msgs = crate::compact::view(&self.history, &self.compactions);
            let request_msgs: Vec<Message> = if self.config.system_prompt.is_empty() {
                view_msgs.clone()
            } else {
                let mut v = Vec::with_capacity(view_msgs.len() + 1);
                v.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: self.config.system_prompt.clone(),
                    }],
                });
                v.extend(view_msgs.clone());
                v
            };
```

`last_prompt` 更新：现有 `self.obs(run_id, ObsBody::LlmCall { usage: t.usage, ... })` 调用点之后加

```rust
            if let Some(u) = t.usage {
                self.last_prompt = Some(u.prompt as u64);
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core`
Expected: 全部 PASS（既有 173+ 测试不得破——视图默认直通）

- [ ] **Step 5: Commit** `feat(core): agent request view through compact::view`

---

## Task 7: agent — run_compaction 引擎（自动 + 手动 + 摘要降级）

**Files:**
- Modify: `crates/crab-core/src/agent.rs`（`run_compaction`、`compact_now`、`compaction_due`、obs 落盘、事件）

- [ ] **Step 1: 写失败测试**（agent.rs tests 追加）

```rust
    use crate::obs::CompactionStrategy;

    /// 一次纯文本 turn，带指定 prompt 数的 usage
    fn usage_turn(prompt_tokens: u32) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta("working".into()),
            StreamEvent::Usage { prompt_tokens, completion_tokens: 10,
                cache_read_tokens: 0, cache_write_tokens: 0 },
            StreamEvent::StreamEnd { reason: StopReason::EndTurn },
        ]
    }

    #[tokio::test]
    async fn auto_compaction_fires_when_prompt_crosses_threshold() {
        // 窗口 100k × 0.8 = 80k。触发用上一轮的 usage 在本轮 loop 顶判定：
        // run0(60k)→run1(80k，不严格大于)→run2(100k)；run3 的 loop 顶触发。
        // 注意 plan 按 prompt 增量计费：平的序列会让后缀"免费"，所以 usage 必须递增。
        let provider = MockProvider::new(vec![
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
            vec![ // 摘要调用（need_summary 成立时消费）
                StreamEvent::TextDelta("summary of earlier work".into()),
                StreamEvent::StreamEnd { reason: StopReason::EndTurn },
            ],
            usage_turn(50_000),
        ]);
        let mut config = AgentConfig::default();
        config.context_window = Some(100_000);
        config.auto_compact_threshold = 0.8;
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("start".into()).await.unwrap(); // turn0: 60k
        agent.run("more".into()).await.unwrap();  // turn1: 80k（不触发）
        agent.run("again".into()).await.unwrap(); // turn2: 100k
        agent.run("third".into()).await.unwrap(); // loop 顶：100k > 80k → 压缩
        let bodies = obs_bodies(&agent);
        assert!(bodies.iter().any(|(_, b)| matches!(b, crate::obs::ObsBody::Compacted { .. })),
            "expected a Compacted event, got {bodies:?}");
        assert_eq!(agent.compactions.len(), 1);
    }

    #[tokio::test]
    async fn compact_now_sheds_and_appends_only_one_obs_line() {
        // run0 用带 tool 的脚本（产生 ToolResult 供 shed），其第二次调用接 usage_turn(60k)；
        // run1/run2 各一次 usage_turn。usage 递增 → plan 的增量计费有效。
        let provider = MockProvider::new(vec![
            tool_turn(),
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
            vec![ // need_summary 成立时的摘要调用
                StreamEvent::TextDelta("summary text".into()),
                StreamEvent::StreamEnd { reason: StopReason::EndTurn },
            ],
        ]);
        let mut config = AgentConfig::default();
        config.context_window = Some(10_000); // 小窗口 → keep_from > 0
        config.auto_compact_threshold = 0.0;
        let tools = ToolRegistry::new();
        tools.register(Box::new(Noop));
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let path = session.path().to_path_buf();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("one".into()).await.unwrap();
        agent.run("two".into()).await.unwrap();
        agent.run("three".into()).await.unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let report = agent.compact_now().await;
        assert!(report.turns_affected > 0, "nothing compacted");
        let after = std::fs::read_to_string(&path).unwrap();
        // 红线：日志 append-only——after = before + 恰好一行
        assert!(after.starts_with(&before), "log prefix violated");
        let added = after[before.len()..].trim();
        assert_eq!(added.lines().count(), 1, "expected exactly one new line");
        assert!(added.contains("compacted"), "{added}");
        // shed 生效：视图里出现占位符（块不删，只换内容——视图长度不变）
        let v = crate::compact::view(agent.history(), &agent.compactions);
        assert_eq!(v.len(), agent.history().len());
        assert!(v.iter().flat_map(|m| &m.content).any(|b| matches!(b,
            crate::types::ContentBlock::ToolResult { content, .. } if content.contains("[shed:"))));
    }

    #[tokio::test]
    async fn summary_failure_falls_back_to_shed_only() {
        // 构造 need_summary 的工况，摘要调用用空脚本 mock（正常返回）；
        // 失败路径用返回 Err 的 provider：先正常跑一轮，再 set_provider 成 Fail 后 compact_now
        struct Fail;
        #[async_trait::async_trait]
        impl crate::provider::Provider for Fail {
            async fn complete(&self, _: &[crate::types::Message], _: &[serde_json::Value],
                _: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamEvent>>)
                -> Result<crate::types::AssistantTurn, crate::provider::ProviderError> {
                Err(crate::provider::ProviderError::Http("boom".into()))
            }
            fn label(&self) -> &str { "fail" }
            fn model(&self) -> &str { "fail" }
        }
        let provider = MockProvider::new(vec![
            usage_turn(60_000),
            usage_turn(80_000),
            usage_turn(100_000),
        ]);
        let mut config = AgentConfig::default();
        config.context_window = Some(10_000);
        let tools = ToolRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(dir.path()).unwrap();
        let mut agent = Agent::new(Box::new(provider), tools, session, config);
        agent.run("x".into()).await.unwrap();
        agent.run("y".into()).await.unwrap();
        agent.run("z".into()).await.unwrap(); // 3 对 → plan 不会前置返回
        agent.set_provider(Box::new(Fail));
        let report = agent.compact_now().await;
        assert_eq!(report.strategy, CompactionStrategy::Shed);
        assert!(report.summary_failed); // 降级可见
    }
```

`compact_now` 返回类型（agent.rs 新增，公开）：

```rust
/// 一次压缩的执行结果（TUI 通知用；事件走 AgentEvent::Compacted）。
#[derive(Debug, PartialEq)]
pub struct CompactReport {
    pub strategy: CompactionStrategy,
    pub shed_tool_results: u32,
    pub turns_affected: u32,
    /// 计划摘要但调用失败，退回 shed-only（spec §6）。
    pub summary_failed: bool,
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core compact_now_sheds`
Expected: FAIL（`compact_now` 未定义）

- [ ] **Step 3: 实现**（agent.rs）

```rust
    /// 手动 /compact 入口：无视阈值，立即执行一次压缩（spec §4）。
    pub async fn compact_now(&mut self) -> CompactReport {
        self.run_compaction().await
    }

    fn compaction_due(&self) -> bool {
        self.config.auto_compact_threshold > 0.0
            && match (self.last_prompt, self.config.context_window) {
                (Some(p), Some(w)) => p as f64 > self.config.auto_compact_threshold * w as f64,
                _ => false,
            }
    }

    async fn run_compaction(&mut self) -> CompactReport {
        let sizes = crate::meter::prompt_sizes(self.session.entries());
        let budget = crate::compact::Budget {
            trigger: self.config.auto_compact_threshold,
            keep: 0.4,
            window: self.config.context_window,
        };
        let plan = crate::compact::plan(&self.history, &sizes, &budget);
        if plan.keep_from == 0 {
            return CompactReport {
                strategy: CompactionStrategy::Shed,
                shed_tool_results: 0,
                turns_affected: 0,
                summary_failed: false,
            };
        }
        // 先确定性瘦身变换，计数；need_summary 时再尝试摘要
        let prefix = &self.history[..plan.keep_from];
        let mut names = std::collections::HashMap::new();
        for m in prefix {
            for b in &m.content {
                if let ContentBlock::ToolUse { id, name, .. } = b {
                    names.insert(id.clone(), name.clone());
                }
            }
        }
        let shed_count = prefix
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count() as u32;
        let turns_affected = (plan.keep_from / 2) as u32;

        let mut strategy = CompactionStrategy::Shed;
        let mut summary = None;
        let mut summary_failed = false;
        if plan.need_summary {
            let shed_prefix: Vec<Message> =
                prefix.iter().map(|m| crate::compact::shed_message(m, &names)).collect();
            let mut req = vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: SUMMARY_PROMPT.into(),
                }],
            }];
            req.extend(shed_prefix);
            match self.provider.complete(&req, &[], None).await {
                Ok(t) => {
                    let text: String = t
                        .message
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.trim().is_empty() {
                        summary_failed = true;
                    } else {
                        strategy = CompactionStrategy::Summarize;
                        summary = Some(text);
                    }
                }
                Err(_) => summary_failed = true,
            }
        }

        // up_to = 日志里第 keep_from-1 条 Message 的 EntryId；找不到（坏日志）
        // 则放弃本次压缩——绝不落一条指向不存在条目的水位线。
        let Some(up_to) = self
            .session
            .entries()
            .iter()
            .filter_map(|e| match e {
                crate::session::SessionEntry::Message { id, .. } => Some(*id),
                _ => None,
            })
            .nth(plan.keep_from - 1)
        else {
            return CompactReport {
                strategy: CompactionStrategy::Shed,
                shed_tool_results: 0,
                turns_affected: 0,
                summary_failed: false,
            };
        };

        if let Err(e) = self.session.append_obs(crate::obs::ObsEvent {
            run_id: self.next_run_id,
            ts: crate::util::now_ms(),
            body: crate::obs::ObsBody::Compacted {
                up_to,
                strategy,
                shed_tool_results: shed_count,
                turns_affected,
                summary: summary.clone(),
            },
        }) {
            eprintln!("[crab] obs append failed: {e}");
        }
        self.compactions.push(crate::compact::AppliedCompaction {
            keep_from: plan.keep_from,
            strategy,
            summary,
        });
        let report = CompactReport { strategy, shed_tool_results: shed_count, turns_affected, summary_failed };
        let _ = self.events_tx.send(AgentEvent::Compacted {
            strategy,
            shed_tool_results: shed_count,
            turns_affected,
        });
        report
    }
```

`SUMMARY_PROMPT` 常量（agent.rs 顶部）：

```rust
/// One-shot summary instruction (spec §6). Kept short and concrete: the
/// summary replaces history, so it must carry decisions, not flavour.
const SUMMARY_PROMPT: &str = "\
Summarize the following conversation so work can continue from your summary \
alone. Keep, with specifics: the user's goals, decisions made and why, files \
modified, commands run and their outcomes, pending tasks, and key technical \
findings (error messages, APIs, formats). Drop pleasantries and restatements. \
Under 800 words.\n\n[Conversation to summarize]";
```

实现注意：`self.obs()` 同文件内已有取当前时间的 helper——`ts` 字段照抄 `obs()`
的实现方式（保持单一先例），不要新造时间函数。

`AgentEvent` 加变体（agent.rs:22 枚举内）：

```rust
    /// Compaction condensed the request view (auto or /compact). The TUI
    /// surfaces it as a notice; the durable record is the Compacted obs event.
    Compacted {
        strategy: CompactionStrategy,
        shed_tool_results: u32,
        turns_affected: u32,
    },
```

`run_inner` 循环顶部（abort 检查之后、`TurnStarted` 发送之前）插触发：

```rust
            if self.compaction_due() {
                self.run_compaction().await;
            }
```

（summary 调用本身就是一次 provider.complete——MockProvider 会 pop 一个脚本；自动触发的既有测试若因多 pop 一次脚本而脚本耗尽，MockProvider `unwrap_or_default()` 返回空脚本不 panic，但断言 usage 的测试需检查——`auto_compaction_fires_when_prompt_crosses_threshold` 已按此设计。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): run_compaction engine — auto trigger, manual /compact path, summary fallback`

---

## Task 8: config — auto_compact_threshold

**Files:**
- Modify: `crates/crab-core/src/config.rs`（Config 字段 + 解析方法）

- [ ] **Step 1: 写失败测试**（config.rs tests 追加）

```rust
    #[test]
    fn auto_compact_threshold_defaults_and_overrides() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c.auto_compact_threshold(), 0.8); // 默认
        let c = Config::from_toml("auto_compact_threshold = 0.5").unwrap();
        assert_eq!(c.auto_compact_threshold(), 0.5);
        let c = Config::from_toml("auto_compact_threshold = 0").unwrap();
        assert_eq!(c.auto_compact_threshold(), 0.0); // 显式关闭
    }
```

（env 覆盖测试不写——`CRAB_AUTO_COMPACT_THRESHOLD` 走 `std::env::set_var` 会并行测试打架；解析函数拆成 `fn resolve_threshold(file: Option<f64>, env: Option<String>) -> f64` 纯函数单独测。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-core auto_compact`
Expected: FAIL

- [ ] **Step 3: 实现**（config.rs）

```rust
pub struct Config {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderProfile>,
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, McpServerConfig>,
    /// Auto-compact when the last prompt exceeds this fraction of the context
    /// window. Omitted = 0.8; 0 = disabled. Env CRAB_AUTO_COMPACT_THRESHOLD wins.
    #[serde(default)]
    pub auto_compact_threshold: Option<f64>,
}

impl Config {
    /// Effective threshold: env > config file > 0.8 default (0 = disabled).
    pub fn auto_compact_threshold(&self) -> f64 {
        resolve_threshold(
            self.auto_compact_threshold,
            get_env("CRAB_AUTO_COMPACT_THRESHOLD"),
        )
    }
}

fn resolve_threshold(file: Option<f64>, env: Option<String>) -> f64 {
    if let Some(s) = env {
        if let Ok(v) = s.parse() {
            return v;
        }
    }
    file.unwrap_or(0.8)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-core config`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(core): auto_compact_threshold config with env override`

---

## Task 9: app 接线 + TUI /compact

**Files:**
- Modify: `crates/crab-cli/src/app.rs`（build_agent → AgentConfig）
- Modify: `crates/crab-cli/src/tui/state.rs`（SlashCommand::Compact + parse）
- Modify: `crates/crab-cli/src/tui/mod.rs`（UiCommand::Compact 分发 + Compacted 事件通知）

- [ ] **Step 1: 写失败测试**

state.rs tests 追加：

```rust
        assert_eq!(parse_slash("/compact"), Some(SlashCommand::Compact));
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p crab-cli parse_slash`
Expected: FAIL

- [ ] **Step 3: 实现**

state.rs `SlashCommand` 加 `Compact` 变体；`parse_slash` match 加 `"compact" => SlashCommand::Compact`。app.rs `build_agent` 的 `AgentConfig` 构造（现有只有 `system_prompt`）扩为：

```rust
    let agent = Agent::new(
        provider,
        tools,
        session,
        AgentConfig {
            system_prompt,
            context_window: resolved.context_window,
            auto_compact_threshold: config.auto_compact_threshold(),
            compactions: if continue_ {
                crab_core::compact::resolve(session_entries_before_move.as_slice())
            } else {
                Vec::new()
            },
        },
    );
```

注意：`resolve` 必须在 session 被 `Agent::new` 按值消费**之前**调用（plan 注意点①）——在 Task 6 已加的 `context_used` 折叠处一并算好：

```rust
    let session_entries_snapshot: Vec<crab_core::session::SessionEntry> = if continue_ {
        session.entries().to_vec()
    } else {
        Vec::new()
    };
    let context_used = if continue_ {
        crab_core::meter::context_used(&session_entries_snapshot)
    } else {
        None
    };
    let compactions = if continue_ {
        crab_core::compact::resolve(&session_entries_snapshot)
    } else {
        Vec::new()
    };
```

mod.rs：`UiCommand` 加 `Compact` 变体；select loop 加 arm（在 `ToggleYolo` 旁）：

```rust
                    UiCommand::Compact => {
                        let report = agent.compact_now().await;
                        if report.turns_affected == 0 {
                            state.notice("nothing to compact".into());
                        }
                        // 非零情况由 AgentEvent::Compacted 处理器统一通知
                    }
```

事件转发处（`AgentEvent` 的 match，与 `Usage` 处理同处）加：

```rust
            AgentEvent::Compacted { strategy, shed_tool_results, turns_affected } => {
                let mut msg = format!("compacted {turns_affected} turns, shed {shed_tool_results} tool results");
                if matches!(strategy, crab_core::obs::CompactionStrategy::Summarize) {
                    msg.push_str(" + summary");
                }
                state.notice(msg);
            }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p crab-cli`
Expected: 全部 PASS

- [ ] **Step 5: Commit** `feat(cli): /compact command and auto-compaction wiring`

---

## Task 10: 收尾

**Files:**
- Modify: `README.md`（config 表 + slash 表 + Sessions 节一段）
- Modify: `STORY.md`（可选：Status 里把 compaction 移出 future work——留到下个 release 统一做，**本任务不动**）

- [ ] **Step 1: README** — config.toml 形状块加一行：

```toml
auto_compact_threshold = 0.8  # auto-compact above 80% of the context window; 0 disables
```

slash 命令表加 `| /compact | compact the conversation now (shed old tool results; summarize if needed) |`；Sessions 节"Observability"子节后加一段：

```markdown
### Compaction

When the last request's prompt crosses `auto_compact_threshold` (default
0.8) of the context window, crab compacts before the next request: old tool
results become short placeholders (re-readable with the tool), and if that
isn't enough, older turns are replaced by an LLM-written summary. The session
log is never rewritten — compaction is recorded as an event and the view is
rebuilt by replay, so `--continue` sees exactly what the model saw.
```

- [ ] **Step 2: 全套件 + clippy**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings`
Expected: 全绿、零警告

- [ ] **Step 3: e2e 冒烟**

```sh
cd "$(mktemp -d)"
# 造一个带大 usage 的会话文件：从仓库已有 session 复制结构，或直接：
mkdir -p .crab/sessions
cat > .crab/sessions/fake.jsonl <<'EOF'
{"kind":"meta","format_version":2,"created":"2026-10-21"}
{"kind":"message","id":0,"parent_id":null,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"kind":"message","id":1,"parent_id":0,"message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}
{"kind":"obs","id":2,"event":{"run_id":0,"ts":1,"obs":"llm_call","provider":"x","model":"x","duration_ms":1,"usage":{"prompt":110000,"completion":1,"cache_read":0,"cache_write":0},"outcome":"ok"}}
EOF
CRAB_CONTEXT_WINDOW=128000 /path/to/crab --provider mock --continue --print "continue" 2>&1 | tail -2
grep -c compacted .crab/sessions/fake.jsonl   # 期望 ≥1
```

期望：自动压缩触发（110k > 0.8×128k），日志新增 compacted 行，--print 正常返回。

- [ ] **Step 4: Commit** `docs(cli): compaction — config, slash command, session-log story`

---

## Self-Review

- **Spec 覆盖**：§2 日志事件（Task 1）/replay（Task 5 resolve + Task 9）/红线（Task 7 compact_now 测试）；§3 模块接口（Task 3/4/5）+ fold 映射（Task 2 prompt_sizes + Task 3 pair_size）；§4 触发配置（Task 7 due + Task 8 config + Task 9 接线 + Task 7 测试）/手动（Task 7 compact_now + Task 9 /compact）/可见性（Task 7 事件 + Task 9 通知）；§5 瘦身规则（Task 4）；§6 摘要（Task 7 SUMMARY_PROMPT + 成功/空文本/失败三路径）；§7 错误表全行有对应测试或分支；§8 测试清单逐项落任务。非目标无任务混入。✅
- **占位符扫描**：无 TBD/TODO；探索性代码段已在写作时落实为最终实现。✅
- **类型一致**：`Budget{trigger,keep,window}` / `Plan{keep_from,need_summary}` / `AppliedCompaction{keep_from,strategy,summary}` / `CompactionStrategy::{Shed,Summarize}` / `CompactReport{strategy,shed_tool_results,turns_affected,summary_failed}` / `AgentEvent::Compacted{strategy,shed_tool_results,turns_affected}` / `Config::auto_compact_threshold()` / `meter::prompt_sizes` / `compact::{plan,shed_message,view,resolve}` —— 全文同名。`SessionEntry::id()`、`util::now_ms`（或等价 helper）、`get_env` 为既有符号，Task 7/8 注明了"复用同文件先例"。✅
