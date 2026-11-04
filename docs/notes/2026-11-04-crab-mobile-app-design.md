# crab 手机 App（PWA + `crab serve`）设计方案 v1

> 前置调研：[2026-11-04 远程控制方案调研](2026-11-04-remote-control-mobile-survey.md)
> （Claude Code / Codex / DSH / Pi 三条路线的信任模型与拓扑）。
> 用户已定的四个边界：① **PWA**（响应式 Web App，可"加到主屏幕"，无原生壳）；
> ② **局域网 + tailnet**（`crab serve` 只绑内网地址，不做公网暴露）；
> ③ **多项目目录**（一个 `crab serve` 管理同一台机器上多个独立 agent，
> 各自有自己的工作目录与 `.crab/sessions`）；④ **全双工**（观察 +
> approve + abort + 发 prompt + steer + `/compact` `/reflect`）。

## TL;DR

`crab serve` 是**用户级常驻进程**：一个 host 上一个，监听局域网/tailnet 地址，
管理一组**工作区**（注册过的项目目录，每个目录一个常驻 Agent）。协议是
**JSONL over WebSocket**（复用现有 `AgentEvent` 形状；命令/响应按 `id` 关联，
事件按 seq 排序——与 Pi RPC 和 Codex app-server 同构，但单连接、无厂商云）。
前端是一个**静态 PWA**（无构建框架的 vanilla TS/JS + manifest + service
worker），由 `crab serve` 自己以 `https://<lan-ip>:7391/` 提供服务；手机浏览器
打开后"加到主屏幕"即得全屏 App。信任模型照抄 DSH 分层但按局域网收窄：
**配对 PIN（一次性、10 分钟有效、最多 5 次尝试）换 device token（长期）+
 自签 TLS（首次连接 TOFU 钉指纹）+ tailscale/内网做网络层准入**。
工作区是唯一的文件系统权限边界：文件工具只接受工作区内的相对路径，
`..` 逃逸一律拒绝。

## 架构

```
┌────────────┐   wss (JSONL)    ┌──────────────────────────────┐
│ 手机 PWA    │ ◄──────────────► │ crab serve (用户级, 常驻)      │
│ (静态, 由    │                  │  ├─ Workspace A: ~/proj-a     │
│  serve 托管) │   LAN/tailnet   │  │   Agent + sessions/        │
└────────────┘                  │  ├─ Workspace B: ~/proj-b     │
        ▲ 推送用周期同步/Poll     │  │   Agent + sessions/        │
        │ (无推送服务, 见 §8)     │  └─ ...                      │
┌────────────┐                  │  TLS 自签 + device tokens    │
│ 局域网内    │ ◄──────────────► │  存 ~/.crab/serve/           │
│ 桌面浏览器  │                  └──────────────────────────────┘
└────────────┘
```

- **为什么用户级而不是项目级**：多工作区共享一个端口、一份 TLS 证书、
  一份配对流程；项目级（每个目录一个 serve）会让"手机管理多台电脑"变成
  N×M 个连接配置，违背 3b 的初衷。
- **为什么 PWA 无推送**：推送需要 APNs/FCM（即厂商云），与"不过境"原则冲突。
  v1 用**前台时即时事件 + 后台周期同步**（PWA 无后台保证，iOS 尤其严格）；
  需要"任务完成提醒"时用 Local Notification（PWA 从后台同步中触发，iOS 16.4+
  支持）兜底。这是一个明确的、写进文档的权衡。

## 组件一：`crab serve`（crab-cli 新子命令）

```bash
crab serve                          # 默认绑 tailscale0 或首个内网地址 :7391
crab serve --host 192.168.1.10      # 显式绑地址（拒绝 0.0.0.0，除非 --public）
crab serve --port 7391
crab serve pair                     # 打印 6 位 PIN + 二维码（终端显示 URL+token）
crab serve devices                  # 列出/吊销已配对设备
```

- **常驻模型**：单个 tokio 进程，内含 workspace registry（内存）+
  持久化存储（`~/.crab/serve/`：`config.toml`（工作区列表、device tokens
  哈希）、`serve-key.pem`（自签 Ed25519，首次启动生成））。
- **单实例保证（serve.lock）**：启动先拿 `~/.crab/serve/serve.lock` 的 OS
  级文件锁（flock/LockFile，内容含 pid+端口+启动时间）。拿不到锁时不报
  错退出，而是转**客户端模式**：读锁内端口、GET /healthz 验证存活，然后
  裸 `crab serve` 提示"already running (pid N, http://addr:port)"退出 0，
  `pair`/`devices`/`workspace add` 等子命令**转发**给已运行实例执行。
  锁随进程死亡自动释放（无陈旧锁问题）。这层保证同时挡住三类真实冲突：
  ① 端口 bind 撞车；② 两个实例 attach 同一工作区——同一 session JSONL
  被两个各自持有分叉内存态（history/compactions）的 Agent 双写，confirm
  双弹；③ `config.toml` 与 audit.jsonl 的 lost-update。
- **附带收益（后续可做）**：普通 `crab`（TUI）启动时查同一锁——若 cwd 已被
  serve 管理，可直接 attach 到 serve 的 agent，根治"同一目录两个 crab"
  的 --continue 撞车同源问题。

### 同一目录：本地 TUI 与远程 App 能同时操作吗？

**默认（v1）不能——因为 TUI 自建 Agent。** 现状：TUI 的 `build_agent` 直接
`Session::create/load` + 自建 Agent；serve 在同一目录也有自己的 Agent。
两个 Agent 对同一 session JSONL 双写、内存态分叉、confirm 双弹——与多开
serve 同源的问题。锁只能防止双开，不能让两个独立 Agent 共存。

**要支持同地双端，TUI 必须改成 attach 模式**（v1.5，改动比看起来小）：
TUI 启动时查 serve.lock，若 cwd 已被 serve 管理，则**不 build_agent，而是
走 ws 客户端 attach 到 serve 的 Agent**。需要动的只有三件事，全是现有
通道的薄封装：

1. **事件源**：TUI 的 `agent_rx = agent.subscribe()` 换成 ws 事件流（同一
   `AgentEvent` 形状，反序列化即可）——TUI 渲染层零改动。
2. **操作面**：`agent.run()`/`compact_now()`/`propose_lessons()` 换成
   同名 RPC——TUI 的 UiCommand handler 已是这层抽象的受益者。
3. **确认**：ConfirmBridge 的 sync channel 改成 RPC confirm_req——现有
   bridge 本来就是"异步源 → sync channel"的适配器，远端只是另一种异步源。

双端语义（协议已含，无需新设计）：
- **并发观察**：多客户端 attach 天然支持（broadcast + 每客户端 seq 游标）。
- **写互斥**：prompt/steer 按到达顺序串行（与 TUI mid-run 排队同语义）；
  **同一 confirm 两个端同时弹**，先答者生效，后到者收 `confirm_resolved`。
- **状态归属**：run_state、compaction、cost 全在 serve 的 Agent 上，TUI
  只是又一个渲染器——所以 TUI 退出再进、手机锁屏再开，视图永远一致
  （attach 重放/增量续传即恢复）。
- **TUI 离线降级**：serve 不在时 TUI 回退到现在的自建模式（单机可用性
  不受 serve 影响）；此时 serve 若启动，同一目录的 TUI 自建 Agent 与
  serve 的冲突由目录锁（见下）挡在启动期。

**目录锁**：除 serve.lock（用户级单实例）外，serve 管理的每个工作区
持有一个 `<dir>/.crab/agent.lock`。TUI 自建模式启动时也拿这把锁——
谁后到谁降级成 attach。这样"同一目录永远只有一个写 Agent"成为系统级
不变量，而不是靠用户记得。
- **Agent 管理**：每个工作区一个 `Agent`（懒加载：首次 attach 时
  `Session::create` 或 `--continue` 最近 session）。与 TUI 共享全部 core
  代码；**不挂 TUI 的 confirm bridge**——serve 直接拦截 gate 决策走
  RPC（见 §协议）。
- **多连接**：同一工作区允许多客户端并发（手机 + 桌面浏览器），事件
  广播天然支持；写操作（prompt/approve）按到达顺序串行（与 TUI 的
  mid-run 排队语义一致）。

## 组件二：协议（JSONL over WebSocket）

每个 WS text frame 一条完整 JSON（UTF-8，`\n` 内嵌转义）。四类记录，
与 Pi RPC 同形：

| 方向 | 记录 | 例子 |
|---|---|---|
| c→s | `cmd`（带 `id`） | `{"id":"7","type":"prompt","workspace":"proj-a","text":"fix the test"}` |
| s→c | `response`（带同 `id`） | `{"id":"7","type":"response","ok":true,"data":{"disposition":"started"}}` |
| s→c | `event`（带 `seq`，工作区作用域） | `{"type":"event","workspace":"proj-a","seq":42,"event":{"kind":"text_delta","text":"…"}}` |
| s→c | `confirm_req`（带 `id`，需回复） | `{"id":"c3","type":"confirm_req","workspace":"a","tool":"bash","summary":"run: rm -rf x"}` |

**命令面（v1 全集）**：
`auth.pair(pin)` / `auth.token(token)` ·
`workspace.list` / `workspace.add(path)` / `workspace.remove(name)` ·
`session.list(workspace)` / `session.create(workspace)` /
`session.resume(workspace, id)` / `session.fork(workspace, id)` ·
`attach(workspace, after_seq?)` / `detach` —— attach 返回
`{head_seq, replay_events[]}`；断线重连用 `after_seq` 增量续传 ·
`prompt(workspace, text, images[]?)` / `steer(workspace, text)`
（mid-run 注入，同 TUI 排队语义）·
`approve(confirm_id, decision)`（allow/deny/allow-all——allow-all 沿用
`gate::ask` 的边界，不碰 allow_all 闩）·
`abort(workspace)` ·
`compact(workspace)` / `reflect(workspace, accept: "all"|"none"|indices[])`
（reflect 的人审在 App 上逐条弹，协议即 confirm_req 复用）·
`fs.read(workspace, rel_path, offset?, limit?)` / `fs.list(workspace, rel_dir)`
· `fs.write(workspace, rel_path, content, create_dirs?)`
（**写文件也走 confirm_req**——与普通工具同一条审批通道）·
`status`（host 名、crab 版本、各 workspace 的 run_state/上下文占用/成本）。

**事件面**：现有 `AgentEvent` 全部透传（TextDelta/ToolCall*/Usage/
Finished/Compacted）+ serve 级事件（`workspace.added`、`agent.idle`
（可用于本地通知）、`confirm.pending` 计数变化）。

**安全边界（写死在 server，不靠客户端自觉）**：
- 工作区注册表是唯一的目录白名单；`fs.*` 与 Agent 的 cwd 都只能是
  已注册目录，路径先 `canonicalize` 再校验前缀。
- `approve` 只对**当前 pending** 的 confirm_id 有效（防重放）。
- 所有 mutation（prompt/approve/abort/fs.write/workspace.add）在
  session JSONL 之外再记一条 serve 自己的 audit obs（
  `~/.crab/serve/audit.jsonl`：谁、何时、什么命令、什么结果）。

## 组件三：PWA 前端

- **技术**：vanilla TypeScript（tsc 无框架）+ 单个 `index.html` +
  `manifest.webmanifest` + `sw.js`。无构建管线——源码即产物，crab serve
  从嵌入的 `include_dir!` 静态目录服务（单文件原则）。
- **页面**（手机宽优先）：
  1. **Hosts**：已配对 host 列表（在线状态、各 workspace 的运行红点）；
     添加 host（输 PIN 或扫二维码——二维码内容即
     `crab://pair?host=…&port=…&pin=…&fp=sha256:…`，指纹内嵌实现 TOFU 零输入）。
  2. **Workspaces**（进入某 host 后）：目录名、状态、最近一条 assistant
     文本预览、未读徽标（自上次查看以来的新事件数）、`+`（输路径注册新
     workspace）——对应 3b 的多目录管理主界面。
  3. **Chat**（进入某 workspace 后）：transcript（复用现有 Bubble 渲染逻辑
     的 Web 移植：user/assistant/tool/notice，文本流式追加，工具块折叠）、
     底部输入框（Enter 发送，Shift+Enter 换行；agent 运行中输入自动
     变 steer）、顶部状态条（模型、上下文 %、成本、Esc=abort 按钮）。
  4. **Approvals**：modal 列表（与 confirm modal 同信息 + 全屏展开命令），
     `[y] 允许 / [n] 拒绝 / [a] 全部允许（本次）`；震动反馈
     （navigator.vibrate）。
- **离线/弱网**：service worker 缓存 shell（HTML/JS/CSS），transcript 数据
  不落 IndexedDB（日志即真相在 host 上，App 只做视图——重放 `attach`
  即可重建，与 crab 的 replay 哲学一致）。
- **通知**：`attach` 里客户端上报 `client.notify: true`；agent 进入 idle
  且页面不可见时，service worker 从 background sync（Chrome）或下次
  前台同步（iOS 兜底）触发 Local Notification。

## 信任模型（分层，每层职责单一）

| 层 | 机制 | 防什么 |
|---|---|---|
| 网络准入 | 只绑内网/tailnet 地址；`--host` 拒绝 0.0.0.0（除非显式 `--public`，且此时强制 TLS+token） | 公网扫描 |
| 传输 | 自签 TLS（wss）；PWA 首次连接弹指纹确认（TOFU），指纹内嵌在配对二维码里时零交互 | 局域网嗅探/中间人 |
| 认证 | 一次性 6 位 PIN（10 分钟、5 次尝试、速率限制）→ device token（32 字节随机，server 存 SHA-256）；吊销即删哈希 | 未配对设备 |
| 授权 | 工作区白名单（唯一目录边界）；`..` 逃逸拒绝；approve 只对 pending id 有效 | 越权访问/重放 |
| 审计 | `~/.crab/serve/audit.jsonl`（每个 mutation 一条） | 事后追责 |

故意不做的（v1）：多用户/RBAC（单用户单设备组语义）、公网 relay、
推送服务、E2E 加密（局域网+自签已够，tailnet 场景下 WireGuard 已加密）。

## 实施路线（按可验证的增量切片）

1. **P0 协议骨架（周级）**：`crab serve` 绑 127.0.0.1 + JSONL/ws +
   单 workspace + `status`/`attach`（只读事件流）+ 最简 HTML 页。
   验收：手机浏览器（同 Wi-Fi）看到 TUI 里正在跑的会话的实时输出。
2. **P1 控制面**：`prompt`/`abort`/`approve`（confirm 走 RPC）+
   workspace.add。验收：手机发起任务、批 bash 权限、Esc 中止。
3. **P2 多工作区 + 会话管理**：workspace registry 持久化、session
   list/create/resume/fork。验收：一部手机管理 host 上 3 个目录。
4. **P3 PWA 打磨**：manifest/service worker/二维码配对/本地通知/
   transcript Web 渲染打磨（宽度、折叠、选择复制）。
5. **P4 steer + compact/reflect**：复用 §协议命令面剩余部分。
   验收：与 TUI 全功能对齐（除 TUI 特有的鼠标选择复制）。

每个切片结束都能 dogfood；P0 之后 App 就已经"能看"，P1 之后"能用"。

## 风险与开放问题

1. **iOS PWA 后台限制**：后台同步不可靠——接受并写明；任务完成通知
   只保证"下次打开时徽标"，不保证实时推送。若此体验不可接受，v2 考虑
   原生壳（回到 1c）或 tailscale 内的 keep-alive 长连接（对 iOS 同样无效，
   故倾向原生壳）。
2. **自签 TLS 在 iOS Safari 的 TOFU**：Safari 不支持程序性钉指纹，
   "指纹确认"是 PWA 内的 UI 流程（比对 serve 上报的指纹与二维码里的），
   不是浏览器级验证——**二维码通道必须可信**（当面扫），文档写清。
3. **多客户端并发 approve**：两个设备同时看到同一个 confirm，先答者生效，
   后到者收到 `confirm_resolved` 事件——协议已含，测试锁定。
4. **工作区路径漂移**：目录被移动/删除时 serve 标记 workspace
   `missing`，保留配置不删（与 session 文件的"不重写"一致），
   用户显式 remove。
