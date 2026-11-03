# crab 记忆系统最小设计（v1）

> 基于三篇调研收敛而成：[pi/dsh](2026-10-23-memory-mechanisms-pi-dsh.md)、
> [Codex/Claude Code](2026-11-03-memory-mechanisms-codex-claude-code.md)、
> [开源记忆系统](2026-11-03-open-source-agent-memory.md)，以及
> [harness² 应用建议](2026-10-23-harness2-application-proposal.md)的 P0-2/P1-3。
> 目标：提取**有效最小设计**——一个 spike（天级）能落地、dogfood 两周后能
> 判断方向是否对的版本。

## 设计原则（每条都有调研依据）

1. **记忆是上下文，不是工具**（pi/dsh：裸工具描述不足以触发记忆使用，
   需要额外指令；first-party 注入天然无此问题）→ lesson 直接进 system
   prompt，不走 MCP 工具。
2. **日志即真相，library 是重放出来的视图**（harness² 提案 + crab 现有
   compaction 重放模式）→ lesson 以 obs event 追加进 session JSONL，
   library = 重放所有 `.crab/sessions/` 的 Lesson 事件得到的派生索引。
   零新写路径，视图永远可重建。
3. **人工指令与机器记忆分层，合计付账**（两家共识的三层模型 +
   P0-1 always-on 预算纪律）→ lesson 注入段与 AGENTS.md、skills 广告共享
   预算，有硬上限。
4. **per-project 作用域**（四家共识：Engram/Memorix 按 Git 项目、Claude Code
   按仓库、开源候选默认按仓库分 bank）→ v1 只注入当前项目的 library。
   全局层是显式设计问题，不允许从项目层顺手长出来。
5. **写入纪律**（Claude Code 官方纪律 + KEEP_BASE）→ 跳过代码/Git/
   AGENTS.md 已能推导的内容；允许空产出；每条 fact 带证据锚点。
6. **可观察、可纠正、人审**（dogfood 哲学 + Claude Code 的 Markdown 可编辑
   + crab append-only）→ 机器提议、人确认后写入；写错不删，追加 supersede。

## v1 范围

**做**：
- `Lesson` obs event 类型（序列化/重放/测试）
- `/reflect` 命令：从本会话轨迹蒸馏 lessons，TUI 确认后追加
- library 重放 + system prompt 尾部注入（带预算上限）
- supersede 纠正路径
- 验收测试脚本

**明确不做**（v1 之后按需再议）：
- 后台自动提炼（Codex 路线）——时延不可控，不符合 dogfood 可观察性
- 全局/跨项目记忆存储（memU 路线）——三个开源候选都证明这需要单独设计
- 语义检索/向量召回——Claude Code 的子串索引 + 按需读已够用
- MCP memory server——crab 已有 client 可随时外挂，不属于 first-party 设计
- 自动写入（无 /reflect 确认）——先把人审路径打磨好

## 数据模型

```rust
// crates/crab-core/src/obs.rs，与 Compacted 同构
Lesson {
    /// facts = 本项目工作区事实（必须含 "in this project" 级措辞或路径锚）
    /// procedures = 可迁移流程经验（跨项目安全）
    kind: LessonKind,            // Fact | Procedure
    text: String,                // 一条一课，单行长度受限（≤ 200 字符）
    evidence: EntryId,           // 锚到本会话触发条目，可追溯、可核查
    supersedes: Option<EntryId>, // 纠正旧 lesson 时指向被替代的 Lesson 条目
}
```

- **library 是派生视图**：重放 `.crab/sessions/*.jsonl` 收集 Lesson 事件，
  按 `supersedes` 链去重（最新存活者入索引）。与 compaction 的
  `resolve()` 同模式，会话文件永不重写。
- 排序：最近存活者优先。视图本身不持久化（或仅作缓存，可随时删了重建）。

## 产生：`/reflect`

1. 用户输入 `/reflect`（或会话结束提示）→ agent 读本会话消息轨迹，
   提议 0..n 条 lessons（prompt 里写明写入纪律：跳过代码/Git/AGENTS.md
   可推导的；fact 必须锚证据；允许空手而归）。
2. TUI 逐条确认（复用现有 confirm 交互模式，同 compaction 人审路径）。
3. 确认后 `append_obs(Lesson)` 进当前 session JSONL。

## 注入：分层 + 预算

system prompt 组装处（`app.rs: build_system_prompt`）在 AGENTS.md 段之后
追加：

```
# Lessons (from previous sessions in this project)
- [procedure] run `cargo insta review` after snapshot tests
- [fact] in this project, sessions live under .crab/sessions
```

- **预算硬上限**：lessons 段总预算 1500 tokens（与 skills 广告共享
  always-on 纪律，超限截断尾部并标注 `… N more lessons on disk`）。
- **全量注入，不做按需读**：lesson 单行 ≤ 200 字符，索引即全文
  （Claude Code 的"索引 + 按需读"在 lesson 短小前提下属于过度设计；
  真长出长文 lesson 时再引入主题文件）。
- fact/procedure 在文本里强制分桶标注——这是未来跨项目只注入
  procedures 的安全前提，v1 就立好规矩。

## 纠正

- `/reflect` 可以提议"替代第 k 条旧 lesson"→ 追加 `supersedes: Some(old)`。
- 人也可以直接删 session 文件里那行 Lesson obs——日志是文本，这是
  append-only 纪律允许的维护操作（与 compaction 的"文件不改、视图重建"
  不冲突：删行等价于该 lesson 从未写入）。
- 注入视图永远以存活 lesson 为准。

## 验收测试（dsh 协议 + 三条质量杠）

对应开源调研的三条验证标准，写成 `scripts/memory_acceptance.sh`：

1. **写入**：会话 A `/reflect` 确认一条 fact → session JSONL 出现 Lesson
   obs，重放后 library 含该条。
2. **召回**：同项目新会话 B → system prompt 含该 lesson（mock provider
   断言注入内容）。
3. **用上**：问一个只有该 lesson 能回答的问题 → 回答确实用上了。
4. **分层**：fact 出现在 procedure-only 消费场景（模拟跨项目注入）时
   必须被过滤——v1 用单元测试锁死过滤规则。
5. **纠正**：supersede 后旧 lesson 不再入索引，新 lesson 存活。
6. **隔离**：项目 X 的 lesson 不出现在项目 Y 的 prompt 里。

## 演进路径（v1 验证之后）

| 信号 | 演进 | 参照 |
|---|---|---|
| dogfood 中重复纠正同类问题 | 会话结束时主动提示 /reflect | Claude Code 会话中更新 |
| lesson 数量超预算、长尾有价值 | 主题文件 + 按需读 | Claude Code MEMORY.md 模式 |
| 跨项目复用诉求真实出现 | 显式设计全局层（只收 procedures + 用户偏好） | memU / Hindsight 的教训 |
| 人工 /reflect 频率太高 | 后台自动提炼候选、人只审 | Codex 路线（时延可接受后） |

## 与 harness² 提案的对应

- 本文 = 提案 P0-2（Lesson event，形状已按调研细化：kind/evidence/
  supersedes）+ P1-3（/reflect + 分层注入）的收敛版。
- 相对提案的变化：① library 明确为"重放派生视图"而非独立存储；
  ② 注入从"facts+procedures / 只 procedures"的两级简化为"当前项目
  全量 + 分桶标注"，把跨项目过滤推迟到全局层设计时再做；③ 验收测试
  从 dsh 三步扩展到含纠正与隔离的六步。
- 建议顺序不变：P0-1（预算纪律）→ 本文 spike → dogfood 两周 →
  按痛点决定演进表中的下一行。
