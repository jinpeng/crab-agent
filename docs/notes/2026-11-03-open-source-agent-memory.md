# 开源 Agent Memory 项目调研

> 截至 2026-11-03，来源：各项目官方仓库与文档（未做安装实测；Stars 为
> 查询快照，只代表关注度）。完整报告：
> `/Users/jinpeng/Documents/Codex/2026-11-03/qin/outputs/agent-memory-research.md`。
> 相关：[2026-11-03 Codex 与 Claude Code 调研](2026-11-03-memory-mechanisms-codex-claude-code.md)
> （harness 内置记忆）、[2026-10-23 pi 与 dsh 调研](2026-10-23-memory-mechanisms-pi-dsh.md)
> （harness + 第三方 memory MCP）。本篇补第三块拼图：**独立开源记忆系统**，
> 调研问题不变：对 crab 的 lesson library（harness² 提案 P1）有什么参照。

## TL;DR

开源记忆系统实际解决的是**三种不同问题**：记住用户（Mem0、memU）、
记住工作（Claude-Mem、Hindsight）、理解持续变化的知识（Graphiti、
Cognee）。这与 harness 内置记忆是不同层次：它们是**记忆基础设施**
（独立进程/服务，通过 API 或 MCP 接入），而 lesson library 是
harness first-party 的上下文层。对 crab 最直接的启示不是选型，
而是它们反复验证的三条验收标准：**个人偏好与项目事实分开、可纠正
旧记忆、准确召回且不带入无关项目内容**——这正好就是 P1 验收测试
该测的东西。

## 项目总览

| 项目 | Stars 约数 | 定位与核心路线 | 更适合的需求 |
|---|---:|---|---|
| [Claude-Mem](https://github.com/thedotmack/claude-mem) | 110.8k | 自动采集编码过程中的工具观察，压缩后检索、注入未来会话 | 给现有 Coding Agent 增加工作记忆 |
| [Mem0](https://github.com/mem0ai/mem0) | 68.3k | 独立记忆层，从交互中提取事实，通过 API 存储与召回 | 给自己的应用增加用户长期记忆 |
| [Hindsight](https://github.com/vectorize-io/hindsight) | 58.1k | 围绕 retain、recall、reflect 组织事实、经历和归纳 | 需要记忆整合、反思及编码工具接入 |
| [Cognee](https://github.com/topoteretes/cognee) | 33.0k | 将文档等数据转换为可检索的图谱与向量表示 | 企业知识、文档和代码上下文 |
| [Graphiti](https://github.com/getzep/graphiti) | 32.5k | 时序知识图谱，跟踪实体、关系和事实变化 | "谁在什么时候发生了什么变化" |
| [Letta](https://github.com/letta-ai/letta) | 25.3k | 有状态 Agent 体系，让代理管理自身记忆 | 构建长期运行、有持续身份的 Agent |
| [memU](https://github.com/NevaMind-AI/memU) | 14.6k | 面向跨 Agent 的个人记忆，当前强调共享个人 Wiki | 多个工具共用个人背景与偏好 |
| [MemOS](https://github.com/MemTensor/MemOS) | 12.2k | 综合记忆管理、混合检索、跨任务技能复用 | 研究或搭建更完整的记忆基础设施 |

- 许可证以 Apache-2.0 为主，Hindsight 为 MIT；**开源代码与其托管服务
  的能力、费用需要分开看**。
- Letta 值得一提：它是"agent 管理自身记忆"的路线，与 Claude Code
  auto memory / lesson library 的取向最接近，但它是完整 agent 框架
  而非可嵌入的记忆层。

## 三种问题空间

1. **记住用户**：偏好、背景、历史决策——Mem0、memU 更直接。
2. **记住工作**：排查过什么、做过哪些选择、哪些方案失败了——
   Claude-Mem 的会话观察路线、Hindsight 的经历与反思机制更值得看。
3. **理解持续变化的知识**：人员关系、业务状态、文档事实如何更新——
   Graphiti（特别强调时间与关系）、Cognee。

lesson library 的对应关系：核心需求是第 2 种（记住工作）， Crab
已有的指令链/skill 部分覆盖第 1 种，第 3 种明确不在 P1 范围。

## 跨项目记忆的三个候选

调研的原始动机是"跨 project 记忆"，三个候选各有侧重：

| 候选 | 值得研究的原因 | 要确认的边界 |
|---|---|---|
| **memU** | 产品方向最直接地面向跨工具个人记忆 | README 快速接入依赖服务 key，完整自托管边界需核实 |
| **Claude-Mem** | 自动捕获工作过程，已覆盖多种 Coding Agent | 跨项目搜索 ≠ 自动区分全局偏好与项目事实；默认安装引导托管 observer 试用 |
| **Hindsight** | 有记忆 bank 和现成 coding 集成，组织方式可控 | coding 集成默认按仓库分 bank，跨项目共享策略仍需设计 |

## 选型判断（原文结论）

- 马上增强 Claude Code / Codex 的工作连续性：先试 **Claude-Mem**。
- 长期建设跨工具的个人记忆中心：重点研究 **memU**，同时比较
  **Hindsight**。
- 为自己的产品加记忆：先看 **Mem0**。
- 需要关系变化和历史溯源：先看 **Graphiti**。

## 真正需要验证的东西

不是"能存多少"，而是：

1. **能否将个人偏好与项目事实分开**；
2. **能否纠正旧记忆**；
3. **能否准确召回，并且不把无关项目内容带进当前任务**。

本次没有做同条件测试，不根据各家宣传基准判断记忆效果排名。

## 对 crab lesson library（提案 P1）的启示

1. **三条验证标准直接变成 P1 验收测试**。dsh 调研给了
   "写入 → 新会话召回 → 真的用上"三步协议；本篇补了质量维度：
   偏好/事实分层、纠正（更新/删除旧 lesson）的读写路径、
   无关项目内容的隔离。比"事件落盘了"严苛得多，也更接近真实使用。
2. **"记住工作"是 lesson library 的差异化定位**。Mem0/memU 做
   "记住用户"，Graphiti/Cognee 做"变化的知识"；"之前排查过什么、
   哪些方案失败了"这层在工作流记忆里反而竞争最少——与 crab 的
   dogfood 日志优势（append-only、可重放）正好匹配。
3. **Claude-Mem 的工具观察路线是零代码中间态的又一选项**。上次
   调研结论是"挂 reference memory MCP 即可有记忆"；Claude-Mem
   说明还有一条"观察工具调用流"的路线， crab 作为 harness 甚至
   可以 first-party 地产生这类观察事件（不用 hook 外部 agent）。
4. **"按仓库分 bank"是默认值，不是终局**。三个跨项目候选全都需要
   额外设计才能共享全局偏好——再次确认 per-project 隔离 +
   显式的全局层是正确结构，全局层要单独设计、不能从项目层"顺手
   长出来"。
5. **托管 vs 自托管的边界问题 crab 没有**。lesson library 是
   纯本地文件（Git 作用域内），不存在服务能力/费用分层，这在
   对照组里是个净优势，值得在 P1 里写明。

## 引用

- 完整报告：`~/Documents/Codex/2026-11-03/qin/outputs/agent-memory-research.md`
- 各项目仓库：见上表链接（Claude-Mem、Mem0、Hindsight、Cognee、
  Graphiti、Letta、memU、MemOS）；Hindsight 介绍另见
  <https://vectorize.io/>。
