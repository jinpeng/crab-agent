# Dogfood Log

记录每次用 crab 改 crab 的过程。最值钱的输入是**卡点**——模型哪里卡了、crab 哪里暴露缺陷。

格式：

## YYYY-MM-DD — <任务>
- 任务: <让 crab 做什么>
- 结果: 成功 / 部分 / 失败
- 权限确认次数: N
- edit 一次过 / 反复 N 次
- 卡点: <模型哪里卡住了>
- 暴露的 crab 缺陷: <要修的>
- commit: <sha 若有>

---

## 2026-10-14 — Session::len + test（Q2 验收）

- 任务: 给 Session 加 `pub fn len(&self) -> usize` 返回 entry 数 + 单元测试，跑测试确认通过
- 结果: **成功** — Q2 验收达成
- commit: f3fec8e
- 暴露的 crab 缺陷: 模型不知道 Rust 惯例 `len_without_is_empty`（加 `len()` 不加 `is_empty()`），clippy `-D warnings` 在 CI/review 抓到，手工补 `is_empty()`（commit a582500）。**prompt 改进点**：BASE_PROMPT 或 AGENTS.md 可加一条"遵循 Rust API 惯例（len 配 is_empty 等），改动后跑 `cargo clippy --all-targets -- -D warnings`"。
- （过程细节待补：权限确认次数 / edit 是否一次过 / 卡点）

## 2026-10-14 — MCP client（stdio，tools-only）

- 任务: 实现 MCP client——spawn stdio MCP server 子进程、list_tools、包成 crab Tool（命名空间 <server>__<tool>）、门禁、启动容错
- 结果: 成功（crab 主导，人修了握手超时）
- 方式: 逐 task 用 `crab --provider deepseek --print "<spec>"` 驱动 crab 实现
- crab 的亮点:
  - Task 1 重构（Tool::name → Cow）机械正确
  - Task 3 自写了 in-test MCP server（手写 JSON-RPC over stdio，测试二进制自 spawn）
  - Task 4 引入 McpSession seam（gate/invoke 分离）让 McpTool 可测不用真 server
  - Task 5 自己写临时假 MCP server 验证了完整链路（注册→模型 tool_call→门禁→真实 MCP 调用→回灌）
- 人修的点:
  - rmcp 握手无超时——非 MCP 进程（/bin/cat）会永远挂。加了 10s HandshakeTimeout。
  - 时序敏感的 self-spawn round-trip 测试标 #[ignore]（--ignored --test-threads=1 跑）
- 暴露的 crab 缺陷: 无（这轮 crab 没撞新鲜度门禁/edit 匹配问题——prompt 强化生效了）
- commits: 77a5f85 (name→Cow), 3491f68 (config mcp), 1d8447c (McpClient), aecc1dc (McpTool), b05b251 (startup wiring)
- release 体积: 8.24 → 10.11 MiB (rmcp)

## 2026-10-14 — 接入 Brave Search MCP server（端到端验证）

- 任务: 配置真实 MCP server（Brave Search）+ 端到端搜索验证
- 结果: 成功（`brave-search__brave_web_search` 返回真实搜索结果）
- 关键发现（crab 用户会撞的坑）:
  1. **Node 的 fetch 不走 `https_proxy` 环境变量**——Node 写的 MCP server（如 brave-search）直连外网会被代理网络挡。修法：MCP server 的 env 里加 `NODE_OPTIONS = "--use-env-proxy"`（Node 24+ 支持）。
  2. crab 的 MCP `env` 字段正好承载这个；且 env 值不进 ps/门禁提示，适合放 API key。
- 用到的 crab 能力: web_fetch 读配置文档、MCP env 配置（ crab 自己加的）、MCP 工具注册/调用/门禁全链路。
- commit: f636f9d (env support)
