# pi 与 DeepSeek Harness 的 memory 机制调研

> 来源：pi coding agent 1.3.0 本地安装（`~/.pi/agent/install/releases/1.3.0/...`，
> docs + dist bundle 全文检索）；`deepseek-ai/deepseek-harness` @ `22ddee37`
>（2026-10-23，shallow clone）。调研问题：两者有没有 memory 机制、什么形态，
> 对 crab 的 lesson library（harness² 提案 P1）有什么参照。

## TL;DR

**两个都没有 first-party memory**。pi 只有会话持久化 + compaction 摘要；
dsh 更进一步——内置了**三个第三方 memory MCP server 的默认关闭参考配置**
和一套接入文档（含验证协议），但记忆本身仍完全外包给外部进程。也就是说
三个系统里，只有 harness² 的 library 是 first-party 结构化记忆设计。对 crab
的启示：MCP 路线零代码即可用上（crab 已有 MCP client），但结构纪律
（fact/procedure 分层、obs 锚定、人审）必须自己建。

## pi 1.3.0：无 memory 功能

全文检索 dist bundle 与 docs（关键词 MEMORY.md / remember / learnings /
long-term / facts / notepad）：

- `MEMORY.md`：0 命中。`remember` 的 6 处命中全部是 X509 信任存储与凭据缓存
  内部（`rememberX509Credential` 等），与用户记忆无关。
- CLI 命令面（`pi --help`）：install/remove/update/list/config/auth/mcp——
  无 memory 相关命令。
- pi 的"记忆"故事 = 三个底层能力，均无跨会话知识蒸馏：
  1. **会话持久化**：JSONL 会话 + `--continue` 恢复（docs/session-format.md）。
  2. **compaction 摘要**：上下文超阈值时总结旧消息腾空间
     （docs/compaction.md）——是上下文恢复，不是知识提取；摘要留在会话内。
  3. **MCP client**：`mcp.json`（agent 目录全局）+ `.pi/mcp.json`（项目级）
     可挂任意 MCP server（docs/configuration.md:15,31）——**挂 memory MCP
     的机制存在，但无任何参考配置或使用引导**。

## DeepSeek Harness（dsh @ 22ddee37）：memory = 第三方 MCP，文档化但外包

核心文件：`docs/user/guide/mcp-memory.md`（+ 中文副本）。

- **形态**：三个"默认关闭的参考配置"（Cordis overlay patch），选一个启用：
  | 系统 | 存储模型 |
  |---|---|
  | Memorix 1.3.0 | 本地启发式，无需 LLM/embedding，按 Git 项目分身份 |
  | MCP Reference Memory 2026.7.4 | 本地知识图谱（entity/relation/observation），JSONL，子串搜索（无语义检索、无遗忘策略） |
  | Engram v1.20.0 | 自带存储，`~/.engram`，从 DSH cwd 自动检测 Git 项目 |
- **DSH 的角色边界**（文档明说）：只解析 overlay、拉起 stdio 子进程或连 HTTP、
  发现 MCP 工具并暴露为 `mcp__<server>__<tool>`。**不**下载 server、不初始化
  数据库、不选模型/embedding、不做数据迁移、不监管 HTTP 服务。
- **引导记忆使用的指令**：可选的厂商中立一句话加进 model instructions——
  *"When the user asks you to remember something, call a memory write tool. When
  historical information may be relevant, search memory and use relevant results."*
  （裸 MCP 工具描述不足以稳定触发记忆读写，需要这层提示——重要发现。）
- **验证协议**（可直接抄作验收测试）：① 会话 A 写入唯一值（"validation
  drink is lapsang-<suffix>"）→ 确认 write 工具被调用且成功；② 同 Host 新建
  会话 B（不复制对话）→ 问该值，确认 search/recall 被调用且返回正确；
  ③ 会话 B 里"用该偏好做推荐"，确认回答真的用上了召回值。
- 既有笔记佐证：`.agents/notes/archived/feature/2026-07-31-third-party-memory-
  mcp-examples.md`（memory 始终是"第三方示例"定位）。

## 对 crab lesson library（提案 P1）的启示

1. **两条路线的明确分野**：
   - **MCP 路线（pi/dsh 同构）**：crab 已有 MCP client，照 dsh 文档模式写个
     参考配置就能用，零代码。代价：无结构纪律（自由 entity/graph）、
     无 fact/procedure 分层、质量取决于外部 server、模型需要额外指令才会
     稳定使用记忆工具。
   - **first-party 路线（harness² 同构）**：Lesson obs event + /reflect +
     分层注入。利用 crab 的 append-only 日志优势（重放即注入源），人审对齐
     dogfood 哲学。
2. **dsh 验证协议直接可用**：P1 的验收测试就按"写入 → 新会话召回 → 真的
   用上"三步写，别测"事件落盘了"就完事。
3. **"需要提示才会用"**：dsh 要加那句厂商中立指令才发现记忆工具被稳定调用
   ——crab 的 lesson 注入是 first-party 直接进 system prompt，天然没有这个
   问题（记忆是上下文不是工具）。这是 first-party 路线的结构性优势。
4. **按 Git 项目分作用域**是两家共识（Engram 自动检测、Memorix 按项目身份），
   与提案的 per-project library 一致，确认了这个方向。
5. **可选的中间态**：先用 MCP 参考配置（比如 reference memory）让 crab 今天
   就有记忆能力，lesson library 作为结构化层后加，两者不冲突（library 进
   system prompt，MCP 记忆走工具）。

## 引用

- pi docs：`configuration.md`（mcp.json 层级）、`compaction.md`（摘要机制）、
  `session-format.md`（会话持久化）；dist bundle 关键词检索（1.3.0）。
- dsh：`docs/user/guide/mcp-memory.md` @ `22ddee37`（三个参考配置、DSH 角色
  边界、指令句、验证协议）；`.agents/notes/archived/feature/2026-07-31-*`。
