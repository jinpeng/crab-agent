# harness² → crab-agent 应用建议

> 基于 [2026-10-23-harness2-research.md](2026-10-23-harness2-research.md) 的调研，
> 把 harness² 的机制映射到 crab 的真实架构（v0.4.1 @ 65cde97）。每条建议标注
> 规模（天/周/月）与涉及的 DESIGN.md 决策。

## crab 已满足的部分（不用做）

- **Skill trigger 行**：harness² 的第一验证规则（"每个 skill 必须在 always-on
  文本里逐字出现"）crab 天然满足——广告即 `read its SKILL.md at <path>`
  （app.rs:80-95），不存在"广告了但不可调用"。
- **格式校验告警**：malformed skills 有 stderr 警告（skills.rs:106-124），
  不是静默丢弃。
- **项目指令**：`AGENTS.md` 已在 system prompt 里（app.rs:76）。
- **日志即真相**：append-only obs + replay 的架构与 harness² 的 ledger/library
  同源——这是下面 P1 能便宜落地的根本原因。

## 明确不做（与 DESIGN.md 或成本结构冲突）

- **WASM/JS plugin API**：DESIGN.md 已明确拒绝；harness² 的 plugin 是
  TS-in-loop，不可移植。
- **全自动 P–P–C 闭环**：harness² 是研究框架（一次改进 = 2k+k 个 agent 会话），
  crab 的 dogfood 是人审闭环，成本结构不同。自动化的最大价值在"无验证器"
  场景，crab 更便宜的等价物是 P2 的 A/B 探针。
- **subagents / 独立 memory 组件**：与单上下文 + compaction 的设计有张力，
  边界真到了再议。

## P0 — 立即做（天级）

### 1. always-on 预算纪律（DESIGN.md 原则 + 广告裁剪）

现状：所有 discovered skills **全量**广告进 system prompt，随 skill 数量
线性膨胀。harness² 的核心原则：always-on prose 是最贵的选项（每个 token 都在
和 agent 的输出预算竞争），加 prose 要删别的付账。

建议：
1. DESIGN.md 记录原则："always-on 按字付费；skill 是默认指令载体；新 prose
   必须说清挤掉了什么。"
2. 实现最小裁剪：skill 广告超过阈值（如 20 个）时截断尾部并加一行
   "N more skills available under ~/.crab/skills — ask to list"，而不是静默
   撑爆上下文。（确定性规则，符合 Q16。）

### 2. Lesson obs event（纯架构，无行为）

以 `Compacted` 为模板加 `Lesson` obs event 类型（`obs.rs`）：

```rust
Lesson {
    project: String,        // 来源项目标识（路径 slug）
    facts: Vec<String>,     // workspace facts，逐条标注 "in this project..."
    procedures: Vec<String>, // 领域通用流程
    evidence: EntryId,      // 锚到触发本会话的条目（可追溯）
}
```

此刻只做序列化/重放/测试，不做产生和消费的代码。append-only 纪律不变：
lesson 写错不重写，写一条新的 supersede（与 compaction 水位线同构）。

## P1 — 核心新能力（周级）：per-project 经验 library

harness² 的 harnessing-level recursion 中，最便宜且对 crab 价值最大的子集：
**会话经验蒸馏 + 跨会话注入**。

### 3. `/reflect` 命令 + library 注入

- **产生**：会话中（或结束时）`/reflect` → agent 从本会话轨迹蒸馏 lessons
  → 人确认后 append `Lesson` 事件。人审是关键（对齐 crab 的 dogfood 哲学：
  机器提议、人接收）。
- **分层注入**（harness² GENERALIZE 的精髓，prompts.py:295）：
  - 同项目会话：注入 facts + procedures；
  - 跨项目：只注入 procedures——并写明"从 library 拷贝的事实必须对照当前
    证据重验证"（防陈旧 workspace fact 跨项目误用）。
- **注入位置**：system prompt 尾部（AGENTS.md 之后），与 skills 广告共享
  always-on 预算（受 P0-1 纪律约束）。
- **与 AGENTS.md 的关系**：AGENTS.md 是人写的静态层；library 是机器写的动态层。
  两者并列，合计付账。
- **KEEP_BASE 纪律**：允许"本会话无 lesson 可写"为空。为凑数写"be thorough"
  式空洞 lesson 是负资产（占 always-on 预算且误导后续会话）。

## P2 — 中期（按需）

### 4. A/B 探针脚本（对比式验证的务实版）

`crab --print` 已有非交互模式。写 `scripts/ab_probe.sh`：同一任务描述 +
固定种子，跑两种配置（prompt 变体 / 阈值 / 模型），diff 两份 session JSONL
的轨迹。用于没有 ground truth 的调参决策（auto_compact_threshold、广告格式
等）——harness² "行为对比替代判分器"的产品化缩小版。

### 5. tool 输出裁剪（内置行为，非插件）

compaction 已做"旧 tool result 换占位符"；harness² plugin 的第一用途
（trim noisy tool output）可以前置：bash/write 返回超阈值时立即截断 +
占位符指向 session 侧全量文件。**边界**：这是内置 config 行为，不是扩展
API；做之前要在 DESIGN.md 补一条决策记录（与已拒的插件 API 划清界限）。

## 顺序建议

P0-1 → P0-2 → P1-3（一个 spike：event 类型 + /reflect + 注入 + 人审确认，
复用 compaction 的全部交互模式）→ dogfood 自用两周 → 按痛点决定 P2。

## 参考

- 调研笔记：[2026-10-23-harness2-research.md](2026-10-23-harness2-research.md)
- harness² 源码：`core/{recursion,prompts,selection,library}.py` @ main
