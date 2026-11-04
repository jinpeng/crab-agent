# 手机端远程控制方案调研：Claude Code / Codex / DeepSeek Harness / Pi

> 截至 2026-11-04。来源：Claude Code 官方 docs（mobile、remote-control 页，
> 抓取快照）、Codex 官方 docs（remote.md、remote-connections、app-server、
> cloud，markdown 快照）、deepseek-harness @ `8bd07339`（本地 clone，docs +
> packages 源码检索）、pi 1.5.0 本地安装（docs 全文检索）。
> 调研问题：各家如何让**手机/浏览器远程控制本机 agent 会话**，方案的形态、
> 信任模型和可达性，对 crab 有什么参照。

## TL;DR

四家分三条路线：

1. **厂商云中转**（Claude Code Remote Control、Codex Remote）：
   本机进程只做出站 HTTPS，注册到厂商云；手机/浏览器经云端与本地会话同步。
   手机能力最强（推送、approve、双端并发），代价是 transcript 过厂商服务器、
   锁账号订阅、锁官方端点。这是"企业可信但数据必须过境"的方案。
2. **自托管 Web UI**（DeepSeek Harness `dsh web`）：
   本机起 HTTP/WebSocket 服务（默认 127.0.0.1:3080），手机浏览器直连；
   公网走用户自己的反向代理（文档专章写清 proxy 必须做的 5 件事）+
   launch token + trusted-host fence。数据完全本地，但暴露面和部署门槛
   交给用户。
3. **可编程协议端点**（Pi RPC、DSH ACP/SDK、Codex app-server）：
   agent 暴露 JSON-RPC（stdio / ws / unix socket），远程客户端自己搭。
   Codex app-server 甚至给了现成的远程 TUI 模式
   （`codex app-server --listen ws://…` + `codex --remote wss://…`）。
   灵活但无手机前端，认证自行负责。

**对 crab 的核心参照**：执行永远在本机，差别只在控制面谁中转。crab 如果做
远程控制，自托管 Web（路线 2）与 DESIGN.md 的"不依赖厂商云、append-only 日志
即真相"最契合；dsh 的 `--public-url` + trusted-host + launch token 三件套是
可以直接照抄的信任模型。

## 一、Claude Code：Remote Control（厂商云中转，出站 only）

来源：docs 页 `/docs/en/remote-control`、`/docs/en/mobile`（抓取快照）。

- **形态**：`claude remote-control`（server 模式）、`claude --remote-control`、
  会话内 `/remote-control`。本机进程**只做出站 HTTPS**，向 Anthropic API
  注册并轮询任务；手机（Claude app iOS/Android 的 Code tab）或
  claude.ai/code 经 Anthropic 云端与本地会话同步。
- **执行位置**：Claude 始终在本机跑——文件系统、MCP servers、工具配置全用
  本地环境；网页/手机只是"本地会话的窗口"。电脑必须开机、进程必须活着。
- **多端并发**：终端、浏览器、手机可以同时发消息，subagent 和工作流进度
  跨端同步；笔记本睡眠/断网自动重连。
- **安全模型**：全程 TLS 经 Anthropic API；多个短期凭证，各自单用途、
  独立过期。连接期间 **transcript 存到 Anthropic 服务器**（用于跨端同步
  和断线恢复），按数据使用政策保留。ZDR/HIPAA 组织不能用。
- **准入门槛**（对我们重要）：Pro/Max/Ultra/Team/Enterprise 订阅；**不支持
  API key**；不支持 Bedrock/Vertex/Foundry、不支持 `ANTHROPIC_BASE_URL`
  指向非官方端点；`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`/
  `DISABLE_GROWTHBOOK` 时不可用。另有可选 Trusted Devices（beta，
  设备绑定+近期认证）。
- **vs cloud sessions**：cloud sessions 跑在 Anthropic 托管基础设施上
  （GitHub repo、关机后继续）；Remote Control 跑在本机。桌面 app 另有
  "Dispatch"（消息式任务），手机可发任务让桌面端自己决定怎么跑。

## 二、Codex：app-server 协议 + Remote connections + Cloud

来源：docs `/codex/remote.md`、`/docs/remote-connections`、
`/codex/app-server.md`、`/codex/cloud.md`（markdown 快照）。

三层，全部公开：

1. **Codex app-server（开源，codex-rs/app-server）**：
   JSON-RPC 2.0（线上省略 `"jsonrpc"` 头），transport 支持
   stdio（JSONL，默认）、WebSocket（每 text frame 一消息）、Unix socket
   （HTTP Upgrade 握手）、off。VS Code 扩展就建立在这上面。
   **自带远程 TUI 模式**：
   ```bash
   codex app-server --listen ws://127.0.0.1:4500
   codex --remote wss://host:4500 --remote-auth-token-env CODEX_REMOTE_TOKEN
   ```
   `--remote` 接受 ws/wss/unix 端点；文档明说 ws 只用于 localhost 或
   SSH 端口转发，**非 loopback 监听 rollout 期默认不认证**，远程暴露前必须
   配 token + TLS。ws 监听还带 /readyz /healthz 健康探针（带 Origin 头的
   请求 403）。另可用 `--code-mode-host https://…` 共享远程执行主机
   （HTTP/2 gRPC），远程必须 HTTPS。
2. **Remote connections（手机端）**：ChatGPT 桌面 app（mac/Windows）做
   host，手机扫码配对 → 手机端可发起/继续 chat、追问、**审批命令**、
   看 diff/测试结果/终端输出/截图、收完成通知、多 host 切换。SSH host 直连
   （读 `~/.ssh/config` 的别名，经 SSH 起远端 app server，要求远端 PATH 有
   `codex`）。工作区管理员可强制 Remote Control 开关。这是**闭环托管**方案，
   手机能力最全（approve、推送）。
3. **Codex Cloud**：任务跑在云端环境（repo + 依赖 + 网络凭证按环境发布），
   网页/手机/桌面/app 都可启动和 review，电脑关机也继续。另有
   "Local computer access with Work Cloud"：任务本体在 OpenAI 云，需要本地
   资源的步骤派发给在线的已连接电脑执行；电脑不可用时新任务可在云容器
   继续（但拿不到本地文件/工具）。

## 三、DeepSeek Harness：自托管 Web UI + ACP/SDK

来源：本地 clone @ `8bd07339`（docs/user/guide/public-deployments.md、
apps/cli/reference、packages/acp、packages/sdk）。

- **`dsh web`**：本机起 HTTP/WebSocket Host，默认 `http://127.0.0.1:3080`，
  服务完整的 Web GUI（apps/web 是 vite 构建的完整前端，apps/desktop 就是
  Electron 包同一套 Web app）。浏览器跨设备（含手机）直连。
- **发布到公网有官方手册**（`public-deployments.md`）：反代必须做到
  ① 原样转发 Host；② 剥掉挂载前缀；③ 转发 WebSocket upgrade；
  ④ 重写 cookie 的 Path/Secure；⑤ 裸 `/ui` 重定向到 `/ui/`。
  信任模型三件套：
  - **`--public-url`**：对外广告地址（仅展示/注入上下文，不授权）；
  - **`--trusted-host`**（可重复）：Host/Origin fence，不在名单里的
    authority 一律 403——"fence 只准入请求，不认证"；
  - **launch token + 签名 session cookie**：打印 URL 里的一次性令牌换
    cookie，"share it only with intended users"。
  TLS 可终止在 proxy 或 listener（`--tls-cert/--tls-key`，listener 模式
  cookie 自动 `Secure`）。文档明确说："Neither the advertised URL nor the
  fence protects the listening port itself, so restrict the port to the
  trusted proxy or network."
- **ACP**（packages/acp）：标准 Agent Client Protocol 的 JSON-RPC stdio
  server——创建/恢复/关闭持久会话、挂 MCP、选模型、发 prompt、收语义更新、
  取消。面向"另一个程序驱动 agent"（子代理、测试 runner、脚本控制器），
  刻意省略 DSH 特有的交互 UI 卡片。
- **SDK**（packages/sdk）：换行 JSON-RPC over stdio，TS client + server +
  Python SDK 同协议；`dsh --profile sdk` 启动。

## 四、Pi：RPC（JSONL over stdio）

来源：pi 1.5.0 本地 docs（rpc.md、rpc-commands.md、sdk.md）。

- `pi --mode rpc --no-session`：长生子进程，stdin/stdout 上 JSONL 命令 +
  响应 + 会话事件流（run/message/tool/queue/compaction/retry）。
  命令异步处理、按 `id` 关联响应；`streamingBehavior: steer/followUp`
  控制运行中消息；extension 命令运行中也可立即执行。
- 严格 JSONL framing（LF 分隔，不要用会把 U+2028/2029 当行界的通用
  readline）；stdout 只走协议，日志走 stderr。
- **没有任何内置网络端点、认证或手机前端**——远程化完全留给宿主自己搭。
  同向的 SDK（Node/Bun 进程内）和 RpcClient（TS 子进程封装）都只为
  集成服务。

## 横向对比

| 维度 | Claude Code RC | Codex Remote | Codex app-server | DSH web | Pi RPC |
|---|---|---|---|---|---|
| 手机可用 | 官方 app，全功能 | 官方 app，全功能+审批 | 无前端，自建 | 移动浏览器 | 无 |
| 执行位置 | 本机 | 本机 / SSH host / 云 | 本机或指定执行主机 | 本机 | 本机 |
| 网络拓扑 | 出站→Anthropic 云 | 出站→OpenAI 云 / SSH | 自己开 ws/unix 监听 | 自己开 HTTP 监听 | stdio 子进程 |
| 认证 | 订阅账号+短期凭证+可选设备绑定 | 账号配对（扫码） | bearer token + TLS（ws 默认不认证！） | launch token + trusted-host fence + TLS | 无 |
| 数据过境 | transcript 存 Anthropic | 经 OpenAI 云 / SSH 直连 | 不过境（自己部署） | 不过境 | 不过境 |
| 断线恢复 | 自动重连 | 自动重连 | 客户端负责 | 浏览器刷新重连 | N/A |
| 厂商锁 | 强（订阅+官方端点） | 强（账号+桌面 app） | 弱（协议开源） | 无（自托管） | 无 |

## 对 crab 的参照

1. **三条路线里，自托管 Web 与 crab 的设计哲学最一致**：不依赖厂商云、
   不过境数据、append-only 日志恰好是 Web 同步的真相源（远端重放即可重建
   视图，与 `--continue` 同构）。
2. **dsh 的信任三件套可以直接照抄**：fence（trusted-host，只管准入）+
   一次性 launch token（换签名 cookie，只管认证）+ TLS（proxy 或 listener，
   只管加密），并且明确"fence 不保护监听端口本身"——分层职责写得非常干净。
   反代五条军规也是现成的文档模板。
3. **Codex 的警告值得记住**：非 loopback WebSocket 在 rollout 期默认
   **不认证**——如果 crab 做 ws 端点，认证必须是 first-class 配置，
   不能后补。
4. **Codex app-server 的远程 TUI 模式是个轻量替代**：如果 crab 不想写
   Web 前端，只做 `crab serve --listen ws://127.0.0.1:PORT`（RPC 风格的
   JSONL over ws，复用 obs 事件流）+ `crab attach wss://…`，手机端用
   SSH 端口转发 + 任意终端就能控制——这正是 Codex 文档自己建议的
   "plain WebSockets only for localhost or SSH port-forwarded" 用法。
5. **Claude Code RC 展示了体验上限**：多端并发同步、断线自动重连、
   手机审批。这些能力 crab 都能从现有架构长出来（confirm bridge 已是
   事件驱动的审批通道，metrics sink 已是广播订阅者），但远程化应该按
   需求驱动——先 `crab serve`（只读观察 + approve）再全双工。

## 引用

- Claude Code：`code.claude.com/docs/en/remote-control`、`/docs/en/mobile`
  （2026-11-04 抓取）
- Codex：`developers.openai.com/codex/remote.md`、`/codex/app-server.md`、
  `/codex/cloud.md`；`learn.chatgpt.com/docs/remote-connections.md`
  （2026-11-04 抓取）
- DeepSeek Harness：github.com/deepseek-ai/deepseek-harness @ `8bd07339`
  —— `docs/user/guide/public-deployments.md`、`apps/cli/reference/README.md`、
  `packages/acp/{,acp/}README.md`、`packages/sdk/README.md`、`apps/desktop/README.md`
- Pi：本地安装 1.5.0 `docs/rpc.md`、`rpc-commands.md`、`sdk.md`
