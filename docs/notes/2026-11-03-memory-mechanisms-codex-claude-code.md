# Codex 与 Claude Code 的 memory 机制调研

> 截至 2026-11-03，来源：OpenAI Docs（Codex Memories、AGENTS.md 加载规则）
> 与 Claude Code 官方 Memory 文档。上一篇：[2026-10-23 pi 与 dsh 调研](2026-10-23-memory-mechanisms-pi-dsh.md)——pi/dsh 两个系统均无 first-party memory；主流 harness 的图景已经改变：
> **Codex 与 Claude Code 都已有“人工维护的长期指令 ＋ 自动积累的记忆”**。
> 调研问题不变：对 crab 的 lesson library（harness² 提案 P1）有什么参照。

## TL;DR

Codex 和 Claude Code 都有 first-party memory，形态都是
**人工指令文件 + 自动记忆**，差异在三点：**记忆如何产生**（后台提炼
历史聊天 vs 会话中直接读写）、**如何组织**（单 store 跨聊天 vs 索引 +
主题文件按仓库隔离）、**作用范围与管理方式**（官方视作内部状态 vs
官方鼓励直接编辑 Markdown）。Claude Code 的"索引 + 主题文件 +
按需加载"结构是 crab lesson library 可以直接对标的 first-party 先例；
Codex 则代表另一条"记忆生成与当前任务解耦"的路线。

## 总览对比

| 维度 | Codex | Claude Code |
|---|---|---|
| 长期指令 | `AGENTS.md`、`AGENTS.override.md` | `CLAUDE.md`、`.claude/rules/`；新版也支持 `AGENTS.md` |
| 自动记忆 | 从符合条件的历史聊天中后台提取、整合 | 工作过程中直接读写记忆笔记 |
| 默认状态 | 本地记忆默认关闭 | 本地会话 Auto memory 默认开启（部分环境例外） |
| 默认存储 | `$CODEX_HOME/memories/`（或 `memories_v2/`，迁移中） | `~/.claude/projects/<project>/memory/` |
| 组织范围 | 本机 memory store，跨聊天复用 | 按仓库隔离，同仓库各 worktree 共用 |
| 管理入口 | `/memories`，读取与贡献可分别控制 | `/memory`，浏览、编辑及开关自动记忆 |
| 人工修改 | 官方建议视为内部状态，通过设置管理 | 官方明确支持直接编辑、删除 Markdown 文件 |

## Codex：从历史工作中后台整理经验

- 开启记忆后，把适合保留的历史聊天转换为本地记忆；**跳过仍活跃或过短的
  会话，等聊天闲置后再处理**，额度不足时也可能跳过。所以"刚告诉它一件事，
  马上开新聊天"不保证已形成记忆。
- **读取已有记忆**与**允许当前聊天贡献记忆**是两个独立开关。
- 生成字段会做秘密信息脱敏。
- 人工规则层：启动时从全局配置目录到项目当前目录构建 `AGENTS.md` 指令链；
  更深目录覆盖前面的规则，同目录优先 `AGENTS.override.md`；默认累计加载
  上限 **32 KiB**。

## Claude Code：代理自己维护的项目笔记库

索引 + 主题文件结构：

```text
~/.claude/projects/<project>/memory/
├── MEMORY.md             # 记忆索引
├── user_role.md          # 用户信息
├── feedback_testing.md   # 纠正与偏好
└── ...                   # 其他主题笔记
```

- 会话开始只自动加载 `MEMORY.md` 前 **200 行或 25 KB（先到为准）**；
  主题文件按需读取——保留多笔记的同时限制启动上下文。
- 记忆在会话过程中更新，文件保留到用户或 Claude 修改、删除为止。
- 四种记忆类型：用户信息、反馈纠正、项目背景、外部参考入口。
- **明确的写入纪律**：跳过可以从代码或 Git 历史推导的信息，以及指令文件
  已有的内容。

## 一个容易过时的认识：Claude Code 也能读 AGENTS.md

新版 Claude Code 支持直接读 `AGENTS.md`；但工作目录及祖先目录存在项目级
`CLAUDE.md`/`CLAUDE.local.md` 时，默认优先走 Claude 指令体系。可配置两者
同时读取，或在 `CLAUDE.md` 中写 `@AGENTS.md` 共享规则。加载语义与 Codex
不完全一致。

## 设计取向

- **Codex ≈ 历史聊天的后台提炼系统**：从过去的工作整理可复用经验，
  记忆生成与当前任务解耦。
- **Claude Code ≈ 代理自己维护的项目笔记库**：何时读、写了什么，可通过
  文件直接观察和调整。
- 两者的记忆都**不等于完整聊天历史，也不修改模型参数**——是外部持久化
  资料，在未来会话中作为上下文使用。官方文档没有两者记忆质量的可比测试，
  机制差异不能直接推出"谁记得更准"。

## 知识分层（两家共识）

| 内容 | 放在哪里 |
|---|---|
| 必须遵守的规范、测试命令、提交要求 | 版本管理中的 `AGENTS.md`／`CLAUDE.md` |
| 用户偏好、反复纠正、阶段性项目背景 | 自动记忆 |
| 多步骤且可复用的工作方法 | Skills |

两家都强调：记忆/指令文本**不能代替强制执行机制**——关键团队规则进仓库
文档，自动记忆负责减少重复解释。

## 对 crab lesson library（提案 P1）的启示

1. **first-party 路线不再是孤例**。pi/dsh 调研时"只有 harness² 做
   first-party 结构化记忆"的判断已过时——Claude Code 就是活先例：
   agent 维护的项目笔记库 + 人可直接编辑。lesson library 的设计方向
   被独立验证。
2. **"索引 + 按需加载"值得直接借鉴**。MEMORY.md 只载前 200 行/25 KB、
   主题文件按需读，与 crab 的 append-only 日志重放思路兼容（lesson
   可以只有索引常驻、全文按需注入），也给"context 占用有上限"提供了
   现成的配额设计参照。
3. **写入纪律可以照抄**：跳过可从代码/Git 推导的信息、跳过指令文件已有
   内容——直接写进 lesson 的准入规则。
4. **Codex 路线作为备选参考**：后台从会话历史提炼（与当前任务解耦）是
   lesson library 之后可能的演进方向（如基于 session 日志的自动
   /reflect），但"闲置才处理、不保证及时"的时延特性也说明它不适合作为
   第一版——Claude Code 的会话中直接写入更贴合 dogfood 的可观察性要求。
5. **知识三层映射到 crab**：仓库规则 → `AGENTS.md`/指令链；偏好与纠正 →
   lesson library（auto memory 层）；可复用方法 → skills（crab 已有）。
   三层边界清晰，lesson library 不该吞并另外两层。
6. **作用域共识扩大**：pi/dsh 之外，Claude Code 也按仓库隔离（worktree
   共用），加上 Engram/Memorix 的按 Git 项目分身份，per-project library
   已是四家共识。

## 引用

- Codex Memories：https://learn.chatgpt.com/docs/customization/memories?surface=app
- Codex AGENTS.md 加载规则：https://learn.chatgpt.com/docs/agent-configuration/agents-md
- Claude Code Memory：https://code.claude.com/docs/en/memory（含 storage
  location、auto memory 类型、AGENTS.md 兼容说明）
