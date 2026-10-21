# ContextMeter — resume 时的确定性 context 占用（replay 测量）

日期：2026-10-21 · 状态：已批准实施 · 前置阅读：`docs/notes/2026-10-19-deepseek-harness-observability.md`（dsh token-meter 分析）、DESIGN.md Q16

## 问题

`--continue` 恢复会话后，状态栏的 `CH<pct>%/<window>` 段是**空白的**，直到新一轮
LLM 调用返回 `usage` 才出现。对长会话的恢复体验差：窗口已知（config），占用已知
（就在日志里），只是没人去读。

现状链路：`AgentEvent::Usage` → `status.add_usage()` → `context_used = 最近一次
prompt_tokens`。这只覆盖"会话进行中"的路径；恢复路径上没有事件源。

## 核心洞察

**上一次请求的精确占用已经在日志里**。session JSONL v2 的 `ObsBody::LlmCall`
带着 provider 实测的四分 usage；`usage.prompt`（含 cache_read，DeepSeek/Kimi 惯例）
就是"当前 context 窗口被占了多少"的权威数字。

dsh 的 token-meter 思想在 crab 的最小落地：**replay 日志做确定性测量**——不需要
tokenizer、不需要估算（Q16 红线：provider usage 是唯一权威锚点），一个纯函数从
`SessionEntry` 流里把这个数字读出来。

## 设计

### crab-core：`meter.rs`（新模块，测量 ≠ 存储）

```rust
/// Deterministic context occupancy measured by replaying the session log.
/// Pure read of what the provider already reported — no model calls,
/// no token estimation (DESIGN.md Q16).
///
/// Returns the prompt-token count of the most recent LLM call that carried
/// usage (prompt includes cache reads — the DeepSeek/Kimi convention).
/// None when the log holds no usage yet: fresh session, a v1 session written
/// before obs events existed, or a session whose calls all lacked usage.
pub fn context_used(entries: &[SessionEntry]) -> Option<u64>;
```

- **反向扫描**：occupancy 是"最近已知值"，v1 不需要累加任何东西。
- `usage: None` 的 LlmCall（mock provider、headers 前中断）：跳过，继续向早找
  （其占用未知，不能用假数填充）。
- 非目标字段（usage 累计、pending 增量、breakdown）一律不算——metrics sink
  已有自己的 live 累计，v1 不统一；没有消费者的数据不产生。

### 接线

1. `app::build_agent`：`Agent::new` 消费 session 之前，`if continue_` 时
   `meter::context_used(session.entries())`，结果存入 `Startup.context_used`；
   新会话为 `None`。
2. `Startup` 加字段 `context_used: Option<u64>`。
3. `TuiState::new` 加第 4 参 `context_used: Option<u64>`，替代现在写死的
   `set_context(None, window)`；11 个测试调用点机械补 `None`。
4. 运行中路径**不动**：`AgentEvent::Usage` 照旧实时刷新。

### 边界与错误处理

| 情况 | 行为 |
|---|---|
| 新会话 | `None` → CH 段隐藏（同今天） |
| v1 会话（无 obs 行） | `None` → CH 段隐藏，不显示假数字 |
| 全部 LlmCall 无 usage | `None` |
| 部分 LlmCall 无 usage | 取最近一次**有** usage 的（反向 find，天然正确） |
| Meta / 消息行 | 忽略 |

### 原则约束

- **测量不改变系统**：meter 是对 entries 的纯函数；日志仍是唯一事实源。
- **诚实显示**：宁可没有 CH 段，不用估算填数（与"未定价显示 '-' 而非 $0"同一文化）。
- **seam 留位**：`meter.rs` 是未来 projection（pending 增量、context breakdown、
  compaction 压力）的落点——共享 fold 架构，但 v1 只建 seam 不建投影。

## 非目标

- usage 四分累计 fold / metrics sink 重构（sink 消费 live 事件，输入类型不同，
  统一的收益不抵改动）
- 下次请求的 pending 增量投影（Q16：估算只用于告警；当前无告警消费者）
- context breakdown（system/tools/messages 构成）
- compaction 集成（未来消费者，届时再设计）

## 测试

- 合成 entries：两次带 usage 的 LlmCall → 返回最后一次的 prompt
- usage=None 的 LlmCall 被跳过，向早找到最近一次有 usage 的
- 只有 Meta/消息行（v1 形态）→ None
- 空 entries → None
- `Session::create` + `append_obs` 真实往返 → fold 结果一致
- TUI：`TuiState::new` 带 seed 值 → 首帧即出现 CH 段
