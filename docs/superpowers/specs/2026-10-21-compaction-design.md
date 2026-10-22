# Compaction 设计 — 混合式：确定性瘦身 + 按需摘要

日期：2026-10-21 · 状态：已批准（机制 C 混合式、触发 C 自动+手动）·
前置阅读：DESIGN.md（Q11/Q16、失败语义表）、`docs/notes/2026-10-19-deepseek-harness-observability.md`（dsh token-meter）、`docs/superpowers/specs/2026-10-21-context-meter-design.md`（meter seam）

## 1. 背景与问题

会话变长后，每次请求的 prompt 逼近 context window，最终 provider 直接报错——
会话作废，这是数据损失。agent loop 的 `request_msgs` 构造点（system + 全量
history）是唯一的发送视图，compaction 就是在这个 seam 上把"全量"换成"压缩视图"。

DESIGN.md v0 明确 punt（"上下文超长：v0 直接报错。不做 compaction"），现在补齐。

## 2. 核心原则：日志即真相，视图可重建

compaction **永不修改、永不删除**已落盘的条目。它只做两件事：

1. 在 session JSONL 末尾追加一条 obs 事件；
2. 替换内存中 `history` 的**请求视图**（磁盘上原封不动）。

`--continue` 时从日志 replay 出同样的视图（walk 所有 `Compacted` 事件依次
apply），确定性重建——这是 dsh "model-visible ⟺ logged" 在 crab 的落地：
模型看到的每一版视图都能从日志解释。

### 日志事件（obs.rs 扩展）

```rust
ObsBody::Compacted {
    up_to: EntryId,            // 水位线：此 id（含）之前的 turns 进入压缩视图
    strategy: CompactionStrategy, // Shed | Summarize
    shed_tool_results: u32,
    turns_affected: u32,
    summary: Option<String>,   // Summarize 时内联存正文（几 KB，保单文件原则）
}
```

多次 compact 共存：每次事件的水位线单调递增（新的 `up_to` ≥ 上次），replay
按序 apply。

## 3. 模块与 seam（深模块）

新模块 `crab-core/src/compact.rs`，接口为两个纯函数：

```rust
/// 纯决策：完整 history + meter 折叠 + 预算 → 压缩计划。不碰 IO、不碰模型。
pub fn plan(history: &[Message], fold: &MeterFold, budget: &Budget) -> Plan;

/// 纯变换：history + 已发生的 Compacted 事件序列 → 发送给 provider 的视图。
/// wire 协议不变式在这里保证（§7）。
pub fn view(history: &[Message], events: &[Compacted]) -> Vec<Message>;

pub struct Budget {
    /// 触发压缩的占用比（auto_compact_threshold，如 0.8）
    pub trigger: f64,
    /// 水位线目标：保留后缀的累计尺寸上限，占窗口比例（如 0.4）
    pub keep: f64,
    /// context window 大小；None 时 plan 退化为字节预算近似（§7）
    pub window: Option<u64>,
}

pub struct Plan {
    /// 水位线：history 中前 keep_from 条消息进入压缩视图
    pub keep_from: usize,
    /// true = 瘦身仍超预算，需要摘要路径（§6）
    pub need_summary: bool,
}
```

**turn 定义**：一个 user 消息（含 tool_result 块）+ 紧随其 assistant 消息的
最小交替单元；`keep_from` 只落在 turn 边界上。占位符里的 "turn N" 编号 =
该处 assistant 消息的序号（从 0 计）。

**fold → 尺寸映射**：`MeterFold` 的 prompt 序列是逐请求的；plan 从尾部反推
——最后一次调用前的 prefix 尺寸已由 `Compacted` 事件界定（§2），水位线只
在界定的后缀内移动，因此每个 turn 的尺寸 = 相邻两次调用 prompt 的差值，
确定性可得。

seam 在 `run_inner` 的 `request_msgs` 构造点。agent loop、TUI、`/compact`
都是这两个函数之上的薄适配器。

`meter` 模块扩展 `MeterFold`：replay 日志得到每 turn 的 prompt 尺寸序列
（`prompt_0, prompt_1 - prompt_0, …`）——context-meter seam 的第一个真实
消费者。

## 4. 触发与配置

- **自动**：每次 LlmCall 后，若 `usage.prompt > threshold × context_window`
  且窗口已知 → 下一 turn 发请求前 compact。`AgentConfig` 补传 `context_window`。
- **配置**：`~/.crab/config.toml` 新增 `auto_compact_threshold: f64`，
  默认 0.8，设 0 关闭；环境变量覆盖 `CRAB_AUTO_COMPACT_THRESHOLD`
  （照 `CRAB_CONTEXT_WINDOW` 惯例）。
- **手动**：`/compact` 斜杠命令，空闲时立即执行同一路径。
- **可见性**：`AgentEvent::Compacted { shed_tool_results, summarized,
  turns_affected }` → TUI 打一行通知，不沉默。

## 5. 瘦身规则（确定性路径，先执行）

**水位线**：从尾部数，保留最小 turn 后缀使其累计尺寸 < 窗口的 40%
（meter 的 per-turn 尺寸给出确定性数值），且至少留 2 个完整 turn。
**永不在 tool_use/tool_result 配对中间切**——水位线只落在 turn 边界
（assistant 消息之后、或 tool_result 消息之后）。

**prefix 的确定性替换**：

| 内容 | 处理 |
|---|---|
| `ToolResult` 正文 | → 占位符：`[shed: read "/x/y.rs" → 12,400 bytes, turn 3; re-read with the read tool if needed]` |
| assistant 文本 | 保留（小，携带决策与结论） |
| 超长 user 文本块（>2KB） | → 前 200 字符 + `…(1,843 chars total, shed)` |
| `ToolUse` args | 保留（小，记录做过什么） |

## 6. 摘要路径（瘦身仍超预算时）

1. 取瘦身后的 prefix，发一次**独立的一次性 complete() 调用**（无 tools、
   非 turn）：固定提示词要求保留 goals / decisions / 改过的文件 /
   未完成事项 / 关键发现，<800 词。
2. 该调用照常写 `LlmCall` obs——**摘要也计费，进 metrics、进 meter**。
3. prefix 整体替换为一条 user 消息 `[Earlier conversation summary]\n…`。
   配对与 user/assistant 交替天然保持（prefix 始于 user 消息、成对结束）。
4. **失败降级**：摘要调用失败 → 退回纯瘦身视图 + TUI 警告；绝不因
   compact 失败中断用户 turn。

## 7. 错误处理

| 场景 | 策略 |
|---|---|
| 摘要调用失败/超时 | 退回 shed-only + 通知 |
| 水位线内已无可 shed（会话极短） | 不 compact，通知 "nothing to compact" |
| `context_window` 未知 | 自动触发跳过；手动 `/compact` 用字节预算近似 |
| v1 旧会话 resume | 日志无 Compacted 事件 → 全量视图，行为同今天 |
| plan 判定需摘要但摘要调用失败 | 落盘事件为 `strategy: Shed`（退回瘦身视图），与 §6 一致 |

## 8. 测试

重心在纯函数（`plan` / `view` 不依赖网络、文件、模型）：

- `plan`：水位线计算、最小 2-turn 保留、预算边界、配对中不切
- `view`：shed / summarize / replay 三种形态
- **view wire 有效性不变式**（每个 view 测试都要断言）：视图以 user 起始、
  每个 tool_use 后紧跟同序 tool_result。允许连续 user 消息——系统注入与
  摘要消息都产生 user,user（OpenAI/DeepSeek/Kimi wire 均接受；严格交替
  从来不是真不变式，实现时修正）。
- **replay 确定性**：增量 apply（事件逐个到达）== 全量 rebuild（resume 时）
- **红线**：compact 前后 session 文件除新增 obs 行外逐字节不变
- 摘要失败降级：view 退回 shed-only，落盘事件 strategy = Shed
- obs 事件 serde 往返

## 9. 非目标

- 跨会话 compaction
- session fork/branch 感知（fork 是未来功能，compaction 事件按 EntryId
  寻址，届时兼容）
- 压缩进行中的 turn
- 图片内容
- 压缩后旧工具结果的"惰性重取"（模型自己可以用 read 重读）
