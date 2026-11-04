# crab serve 拉取式架构设计（Kafka 风格翻转）

> 前置：[2026-11-04 手机 App 设计](2026-11-04-crab-mobile-app-design.md)
> （PWA + ws 推送版）。真机验证暴露 WebSocket 推送路径的结构性问题
> （手写 101 失败、send 背压饿死 confirm_req、confirmer spawn 静默失败——
> 三个独立 bug 链）。本文档是**架构级翻转**：从"服务端推"改为
> **客户端拉 + 各自记 offset**，日志是唯一真相源。经 grilling 收敛
> （Q1-Q12 全部决策见文末附录）。

## TL;DR

serve 不再用 WebSocket 推送事件。改为：**所有 session 消息和事件以统一
EntryId 追加进 append-only JSONL 日志**（现有格式，id 单调连续已验证）。
客户端通过**长轮询 HTTP**（`GET /events?after_id=N&timeout=30s`）拉取，
**各自记录自己的 offset**（localStorage），断线重连从自己的 offset 续拉。
审批等控制命令走普通 HTTP POST。未决审批作为**状态快照**（不是事件）在
attach/hello 响应里携带。服务端彻底无状态于客户端进度——只保证日志
写好了，客户端自己来拉。

**为什么这比 ws 可靠**：ws 的不可靠源于**协议复杂性**（升级握手、帧
协议、Safari 背压/节流）叠加**推送语义**（服务端要跟踪每个客户端的
推送进度）。长轮询把传输降级为普通 HTTP（无握手、无帧、Safari 对
XHR/fetch 的兼容性远好于 ws），把进度跟踪翻转到客户端（服务端无状态）。
代价：每客户端一个常驻 HTTP 连接（hold 期间），网络切换需重连——
但 HTTP 重连比 ws 便宜且无状态。

## 架构

```
┌────────────┐                      ┌──────────────────────────┐
│ 手机 PWA    │  GET /events         │ crab serve               │
│ (offset 存  │ ◄──── events[]      │  ├─ Session JSONL (append)│
│ localStorage│                      │  │  id: 1,2,3... monotonic│
│  自记)      │  POST /prompt        │  ├─ broadcast notify      │
│            │  POST /approve       │  │  (唤醒长轮询)           │
└────────────┘                      │  └─ pending confirms      │
       ▲                             └──────────────────────────┘
       └────── 拉取循环：after_id=上次最大id ──────────────────────────┘
```

**核心不变量**：日志是唯一真相源。客户端是**可丢弃的视图**——任何时候
客户端崩溃/重开，都能从日志重建（从自己的 offset 或从最新）。

## 协议契约

### 拉取事件（长轮询）

```
GET /events?after_id=<u64>&timeout=<secs>&token=<token>

200 OK
{
  "events": [
    {"id": 42, "entry": {"kind":"text_delta","text":"..."}},
    ...
  ],
  "high_watermark": 45,
  "pending_confirms": [{"id": 3, "tool": "bash", "summary": "run: rm x"}]
}
```

- `after_id`：客户端已见的最大 EntryId（0 = 从头，但只返回**最近 200 条**，
  更早用 `before_id` 翻页——防一次拉几千条卡死手机）。
- `high_watermark`：服务端当前最大 id（客户端据此判断落后程度）。
- **hold 语义**：`after_id < high_watermark` 时立即返回已有事件；否则
  hold 到 `timeout`（默认 30s，上限 60s）或新事件到达（取早者），
  空返回 `{"events": [], "high_watermark": M}`，客户端立即重发。
- `pending_confirms`：**未决审批的状态快照**（与 offset 无关）——
  新/重连客户端必须能看到正在等待的审批，不依赖事件回放。

### 控制命令（普通 HTTP POST）

```
POST /cmd {token}
{"type": "prompt", "text": "..."}           → {"ok": true, "disposition": "started"}
{"type": "approve", "confirm_id": N, "decision": "allow"}  → {"ok": true}
{"type": "abort"}                           → {"ok": true}
{"type": "workspace.switch", "name": "..."} → {"ok": true, "active": "..."}
...
```

审批的并发：多个客户端拉到同一 confirm，**第一个 approve 生效**，
其余在**下次拉取**时看到 `confirm_resolved` 事件（或在
pending_confirms 里消失）。`approve` 对非 pending id 返回错误（防重放）。

### 客户端 offset 管理

- **存储**：`localStorage.crabOffset_<host>`，每收到一批事件更新为
  `max(events[].id)`。
- **恢复**：打开时读 localStorage，`after_id = stored`。若服务端
  `high_watermark > stored + 1000`（落后太多，比如关了一周），
  **强制从最新开始**并在 UI 提示"已跳转到最新（落后 N 条）"——
  防一次拉几千条卡死。
- **UI 可见性**（Q3 要求）：界面必须显示"当前查看最新（非完整历史，
  共 M 条，从第 K 条起）"或"完整历史"状态；上滑用 `before_id` 翻更早。

## 审批生命周期（拉取架构下）

1. 工具触发 gate → confirmer 写 `confirm_req` 事件进日志（EntryId=N）
   + 登记到 pending 列表。
2. **所有活跃客户端**在下一次拉取（hold 被唤醒，毫秒级）收到该事件，
   弹出审批卡片。
3. 某客户端 POST `/approve` → 第一个到达的决策生效：
   - confirmer 收到决策，写 `confirm_resolved` 事件进日志；
   - 从 pending 列表移除；
   - 工具继续执行（或拒绝）。
4. 其他客户端下次拉取看到 `confirm_resolved`（或 pending 消失），
   撤下卡片。
5. **客户端断线重连**：hello 响应带 pending 列表，重连即看到未决审批。
   **客户端一直连着但漏看**（iOS 后台暂停）：接受盲区——活跃客户端
   不会漏，被暂停的客户端本来也看不到提醒。无 120s 超时（Q12-b）。

## 为什么这比 WebSocket 可靠

| ws 推送的坑（真机踩过的） | 长轮询如何避免 |
|---|---|
| 手写 101 响应被 iOS Safari 拒 | 无升级握手——普通 HTTP GET |
| `send().await` 被 Safari 背压/节流卡死，饿死后续事件 | HTTP 请求-响应，无长连接缓冲——每个响应要么完整发出要么失败重试 |
| confirmer 里 `tokio::spawn`（同步闭包）静默失败 | confirmer 只写日志（同步）——传输层与 confirmer 解耦 |
| 服务端要跟踪每个客户端的推送进度和连接状态 | 服务端无状态——客户端自己记 offset，自己重连 |

**诚实的边界**：长轮询不是银弹。代价是① 每客户端一个常驻连接
（hold 期间占 worker，个位数客户端可接受），② 网络切换（Wi-Fi↔蜂窝）
TCP 断开需重连（但 HTTP 重连无状态、廉价）。它解决的是**协议复杂性
导致的 bug**，不是网络本身的问题。

## 关键实现决策

- **offset = 现有 EntryId**：已验证单调连续（message 和 obs 统一编号），
  无需新索引。meta 行 id=None 特判为 0。
- **写串行化**：`prompt` 不再 spawn task 竞争 `agent.lock()`——改为
  **per-agent 命令队列**（FIFO），保证日志顺序 = 客户端发送顺序
  （ws 时代就存在的顺序不确定性，在"日志即真相"下必须修）。
- **事件源复用**：现有 `Shared.tx`（broadcast channel）作为"新事件
  通知"——长轮询 endpoint 内部就是 `receiver.recv().await` 包一层
  HTTP，30s 超时。事件进日志的同时进 broadcast，唤醒所有长轮询。
- **ws 彻底删除**（Q8）：不留 fallback。ws 的教训写进 DESIGN.md
  （协议简单性 > 推送语义）。

## 验收（确定性，Q11）

**`scripts/serve_pull_acceptance.sh`**——这是架构正确性的试金石，
必须全绿才算成功：

1. 起 serve（mock provider，确定性事件）。
2. 起 **3 个并发拉取客户端**（各自独立记 offset，模拟手机+桌面+iPad）。
3. 注入 100 个确定性事件（mock 的 tool 调用流）。
4. **断言**：三个客户端都收到完整的 1..100 **连续无丢**序列，
   且各自的 offset 单调递增到 100。
5. **断言**：一个客户端中途断开 5s 重连，从自己的 offset 续拉，
   补全缺口，最终同样收到完整序列。

**如果这个脚本过不了**，说明问题在**事件源**（日志/broadcast）而
不是传输——那换架构就是误诊，必须回查，不能上线。

**真机回归**（第二步）：iPhone 走完整审批流程，卡片**立即**弹
（< 2s，不是 120s）。

## 迁移路径

1. **实现 `/events` 长轮询 endpoint + `/cmd`**（复用现有 dispatch
   逻辑，去掉 ws 帧包装）。
2. **PWA 改为拉取循环**（删除 WebSocket，改 fetch 长轮询 + offset
   localStorage + pending 卡片渲染）。
3. **跑 `serve_pull_acceptance.sh`**（必须全绿）。
4. **真机回归**（iPhone 完整审批流程）。
5. **删除 ws 代码**（handle_ws、wss 端口、tungstenite 依赖），
   DESIGN.md 记录决策。
6. 提交 + push。

## 附录：Grilling 决策记录（Q1-Q12）

- Q1 拉取传输：长轮询 HTTP（非短轮询/SSE）。
- Q2 日志载体：复用现有 session JSONL（不引入 merkql/eventuary-fs，
  offset=EntryId 已验证单调连续）。
- Q3 offset 语义：客户端自记（localStorage），新客户端从最新开始，
  历史用 before_id 翻页，UI 显示是否完整历史。
- Q4 审批呈现：attach/hello 带 pending 状态快照（不依赖事件回放）。
- Q5 长轮询 hold：服务端 hold（broadcast 唤醒），30s 超时。
- Q6 根因诊断：协议复杂性为主、推送语义为辅——长轮询解两者。
- Q7 写并发：per-agent 命令队列（FIFO），修 ws 时代的顺序不确定性。
- Q8 ws 去留：彻底删除，教训写进 DESIGN.md。
- Q9 endpoint 契约：after_id + high_watermark + 最近 200 条 +
  before_id 翻页。
- Q10 offset 存储：localStorage + 落后 >1000 强制从最新。
- Q11 验收：确定性压测脚本（3 客户端 × 100 事件连续无丢）+ 真机回归。
- Q12 审批超时：去掉 120s 超时和重发——活跃客户端不会漏，被暂停的
  客户端催也没用；依赖 hello/attach 带 pending。
