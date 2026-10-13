# crab-agent 设计决策记录

Rust 实现的编码 agent harness，受 pi 启发。终端命令 `crab`。

本文件是**封板的决策记录**，不是待议清单。每条决策附理由，理由存在的意义是防止重新辩论。要推翻任何一条，先说明理由中的哪一点不再成立。

决策编号 Q1-Q21 对应初始设计评审（grilling）的提问顺序，保留以便追溯。

---

## 1. 定位与验收

**Q1 定位**：pi 的精神续作，不是兼容移植。借用它的概念模型（session 树、steering 语义、skills 思路），但自己的格式、自己的扩展机制，**不保证互通**。

- 不搬 pi 的源码、system prompt 文本、session schema 文本。pi 是 MIT，照抄字符串和 schema 是带署名义务的衍生；概念本身不受版权保护。
- 不做 `.pi/` 半兼容——留这种尾巴会让用户期待全兼容。
- 本仓库 Apache-2.0。

**Q2 验收标准**：**某次 commit 的 diff 完全由 crab 自己产生**（dogfood）。

这是唯一无法自欺的标准，并且会自动逼出真实需求（流式中断、长上下文、文件编辑精度），比任何 roadmap 都准。benchmark / star 数 / "学会 Rust" 都不作为验收标准。

**Q4 命名**：crate `crab-agent`，bin `crab`。

- crates.io 上 `crab` 已被占用（2020 年停更的 CIL scraping tool，4541 次下载）。
- `crab-agent` 经查未被占用。
- **达成 Q2 验收前不 publish 到 crates.io**。名字是单向门，占了又弃坑就轮到自己当那个停更的 `crab`。

---

## 2. 架构

**Q19 仓库布局**：workspace 切两个 crate，**只切这一刀**。

| crate | 内容 | 硬约束 |
| --- | --- | --- |
| `crab-core` | agent loop、session、provider、tools、prompt 组装 | **不许出现任何终端相关依赖，不许有 `println!`** |
| `crab-cli` | clap、crossterm、reedline、渲染、权限确认 UI | — |

这条线是 MockProvider（Q15）能成立的前提，也是"fork 并重编译"扩展故事（Q7）能用的前提——嵌入 core 的人不该被拖进终端依赖。单 crate 分模块的问题不是不够优雅，是让 `println!` 泄进 loop 太容易。

不用 `rig-core` 之类的 agent 框架：harness 的全部价值就是你拥有那个 loop。

**Q11 loop 与并发**：agent 跑在独立 tokio task。`mpsc` 进命令 / `broadcast` 出事件 / `CancellationToken` 中断。

- v0 只实现 `submit` + `abort` + 一个 FIFO 队列。**steering 与 follow-up 的语义区分暂不做**——等 dogfood 时真的被"想在它跑错方向时立刻纠正"咬到再分化。
- **中断时已产生的文本和已完成的 tool 结果必须落盘。** 否则 `--continue` 会给 provider 喂一个没有配对 `tool_result` 的孤立 `tool_use`，多数 provider 直接 400。因此 session 格式必须能表达"被中断的 assistant 消息"。
- 取消安全是这一块的真问题，通道结构不是。

---

## 3. Provider 与流式解析

**Q3 provider 面**：先写死一个 **OpenAI-compatible** 实现把 loop 跑通，**接第二家 provider 时才抽 `Provider` trait**。

过早抽象会按第一家 API 的形状把 trait 拧歪。认证 v0 只认环境变量 + `~/.crab/auth.json`；**OAuth 订阅登录明确排除**（法务 + token 刷新的坑，不是技术坑）。

**Q13 SSE 解析**：**不碰 IO 的纯函数状态机**，是整个项目最该被测试覆盖的地方。

OpenAI-compatible 的 tool_call 流式协议比 Anthropic 脆得多：靠数组 `index` 拼接、`arguments` 是裸字符串碎片、`id`/`name` 只在首片出现，而 DeepSeek / Kimi / vLLM / llama.cpp 各有偏差（有的不给 `index`，有的 `finish_reason` 与 tool_calls 同帧，有的在 `[DONE]` 前不发 stop）。所以：

- 宽容累加器：按 `index` 建槽位，**缺 `index` 退化为追加到最后一个槽**。
- `arguments` 收全后才 `serde_json` 解析；解析失败把**原始字符串回灌给模型**让它重试，不要静默丢弃。
- 真实 SSE 字节流存成 fixture 文件（每个 provider 一份），喂给状态机断言输出，不需要网络。

**失败语义**：

| 场景 | 策略 |
| --- | --- |
| 429 / 5xx | 指数退避重试 3 次，**仅在尚未产生任何内容时**。已开始流式后重试会产生重复文本。 |
| 流中断（已收到半个 tool_call） | **不自动重试**。当作用户可见的失败 + 保留部分内容落盘。 |
| 上下文超长 | v0 直接报错。不做 compaction。 |

不用 `async-openai` 等现成 crate 的类型：provider 类型是核心抽象，交给第三方会让你在遇到 Kimi 的偏差时无处下手。

**已知陷阱**：reqwest 0.13 把 TLS feature 从 `rustls-tls` 改名为 **`rustls`**，且 `default-features = false` 时需显式加 **`charset`** 才能解文本编码。

---

## 4. Session

**Q8**：append-only JSONL。entry 带 `id` / `parent_id` 字段，但 **v0 只实现线性追加 + `crab --continue`**，分支留给未来的 reader。

- `parent_id` 现在加几乎零成本，事后补要迁移所有历史文件。
- branch / fork / clone 的 UI 与语义是无底洞，且 Q2 验收完全不需要。
- **不上 SQLite**：它换来的查询能力用不上，换走的是"session 文件能 `tail -f`、能 `git diff`、能手改"——这对调试 agent 太值了。

---

## 5. 工具

**Q6 v0 工具集（6 个）**：`read` `write` `edit` `bash` `glob` `grep`。

**每个工具的描述文本与它的 `Tool` 实现放在同一文件**（Q20）。描述一旦与行为漂移，模型会以你无法调试的方式犯错。

**Q14 `edit` 语义**（v0 最容易出错的一环，值得占一半测试预算）：

- **字面精确匹配 + 要求唯一**，0 个或 >1 个都报错。
- 多处编辑**对原文匹配**，不逐次累加。
- 失败时**全有全无**，不部分应用。
- **新鲜度门禁**：本会话读过且 mtime 未变。挡掉最坏的故障——你在编辑器里改了文件，模型拿着旧内容把你的改动盖掉。
- **坚决排除模糊匹配 / 空白容错**：它把一次响亮的失败换成一次静默的错误编辑，而模型看到 "ok" 就继续往下走了，这在 agent 场景里是灾难。
- `similar` 只用于**生成报错提示**（给出最接近的候选位置），**绝不用于决定改哪里**。

测试矩阵：唯一性、多处对原文匹配、全有全无、新鲜度、CRLF、无尾换行、UTF-8 多字节边界、超大文件。

**`bash`**：

- 必须 `kill_on_drop(true)`。`tokio::process::Command` 默认**不会**在 future 被 drop 时杀子进程，否则 abort 之后会留下一堆仍在跑的 `npm run build`。
- 超时。
- 输出截断：保留头尾、中间省略并标注省略字节数。

---

## 6. 权限（唯一的安全决策）

**Q9**：门禁放在**工具调用层**，不解析 shell 命令字符串。读类自动放行 / 写·网络·删除类弹确认 / `--yolo` 全放行 / 会话内"本次全放行"。

**README 必须明确写出：这不是安全边界。** 任何基于命令字符串的 allowlist 都可绕过——`bash -c`、`eval`、`$(...)`、写个脚本再执行、`find -exec`。这套门禁的定位是**防手滑、防模型犯蠢**，真正的隔离只有容器。

**不做 OS 沙箱**（macOS `sandbox-exec` 已 deprecated、Linux landlock/seccomp 另一套）：两套平台代码换一个半真半假的沙箱 + 大逃逸面，是性价比最差的一项。

一个没有门禁的 bash 工具等于让 LLM 以你的身份执行任意命令，包括 `rm -rf`、读 `~/.ssh`、往外发代码。这就是为什么门禁必须进 v0。

---

## 7. 扩展

**Q7**：v0 只有 **Rust trait**（`Tool` / `EventHandler` / `Renderer`）。外部扩展的**唯一机制是 MCP**（`rmcp` v3.5.1，官方 Rust SDK，90 天 1900 万下载）。

把"扩展"和"MCP server"合流，白捡整个 MCP 生态，不用发明 ABI。

**明确放弃**：

- **嵌 JS**：rquickjs 轻但无 async、无 npm；deno_core 能跑 npm 但把二进制拖到 50MB+，直接废掉体积预算。
- **WASM（Wasmtime / component model）**：WASM 插件画不了 TUI、拿不到 session 事件流的原生对象，而且没人会为一个新 agent 写 WASM——结果是一个零插件的插件系统。

进程内扩展（能改 TUI、能拦截事件）在 Rust 里的诚实答案是 **fork 并重编译**。6 MiB 单文件 + 秒级增量编译，这个答案不丢人。

---

## 8. 终端与 CLI

**Q10**：**v0 用 crossterm 流式打印 + reedline 输入行**，后续升级到 ratatui `Viewport::Inline` + `insert_before`。

**明确排除 ratatui 全屏 alt-screen app**：它接管滚动与选择复制，于是你被迫自己实现 scrollback、鼠标选择、搜索——好几周工作量，且与 Q2 验收零关系。

从 crossterm 流式打印升级到 `Viewport::Inline` 是**增量的**；跳到 alt-screen 是**重写**。流式打印的额外好处：天然可重定向、可 `| tee`。

**Q12 配置与上下文文件**：读 **`AGENTS.md`**（跨 agent 事实标准），配置放 `.crab/`，格式 **TOML**。

- **不发明 `CRAB.md`**：用户已经在维护 AGENTS.md，让他再维护一份只会被忽略，是纯负和博弈。
- TOML 而非 JSON：JSON 不能写注释，而 agent 配置文件里注释很重要。

**Q20 system prompt**：base prompt + 各工具自带描述 + `AGENTS.md` 追加。用户定制只给 `.crab/system.md` **追加**，**不给替换**——完全替换让用户能静默掐掉工具协议，然后把 bug 记在 crab 账上。

---

## 9. 测试

**Q15**：

| 层 | 手段 | 覆盖 |
| --- | --- | --- |
| loop | **`MockProvider`**（主干，无网络、毫秒级） | 多轮、tool 报错、abort 后的落盘形态 |
| SSE 解析 | fixture 文件 | 各 provider 的协议偏差 |
| 传输层 | `wiremock` 0.6.5 | 429 退避、连接中途断开、缺 `[DONE]` |
| CLI 契约 | `assert_cmd` 2.2.2 | `--print` 端到端 |
| 快照 | `insta` 1.49 | 渲染输出、报错文案 |

**CI 不打真实模型**：它测的是模型而不是你的代码，会 flake、会花钱，且失败时无法区分"我写错了"和"模型今天笨"。留一个手动的 `cargo run --example live-smoke` 肉眼看。

**`--provider mock` 要留一个永久入口**，它在 dogfood 之后仍是最快的调试手段。

---

## 10. 性能预算

**Q5**：性能指标要立（防回归，便宜），但**不把"快 N 倍"当项目理由**。卖点是"可控、可改"。

实测依据：pi 1.2.0 热启动 **0.15s**（本机三次 0.23 / 0.15 / 0.15），也就是说启动优化的天花板只有 140ms。用户感知里 150ms 和 15ms 的差别远小于"tool 调用是否可靠"。（pi 安装目录 148MB 含 node_modules，与"单文件二进制"不是同一量纲，不作为对比依据。）

**Q17 budget 怎么量**——分界线是**确定性**：

- **二进制体积进 CI 做硬断言**：同样代码在 CI 上永远编出同样字节数。**v0 做完先实测，把实测值 +20% 冻结成上限。**
- **启动时间与 idle RSS 只放本地脚本**（`make perf`）。共享 CI runner 上启动时间抖动可达 2-3 倍，一个 `hyperfine` 阈值就是个 flake 发生器。

**实测基线**：v0 全套依赖（tokio / reqwest+rustls / eventsource-stream / reedline / crossterm / similar / globset / grep-* / clap / serde / toml / anyhow / thiserror）构成的 probe，在下列 profile 下为 **6.02 MiB / 176 crates**。v0 落地预期 7-9 MiB。

**release profile**：

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
# 不设 panic = "abort"
```

**为什么保留 unwind**：`panic = "abort"` 与 Q11 冲突。它让 tokio 失去 unwind 捕获能力——`edit` 里一个切片越界 panic 本应变成 `JoinError::Panic` 回灌给模型当一次工具失败，结果会变成整个进程 abort、会话丢失。换来的体积收益只有 5-10%，不值。

TLS 用 **rustls** 而非 native-tls：少一个系统依赖，静态链接更干净。

**Q18 分发**：只 `cargo install --git`。等有第二个人想用再加 GitHub Releases + `install.sh`。macOS 签名公证是纯税收，现在不交。

---

## 11. v0 围栏

**进 v0**：交互 loop、6 个工具、session 追加 + `--continue`、`AGENTS.md`、`--print` 一次性模式。

`--print` 进来的理由不是功能，是**它让 crab 可脚本化、可被 `assert_cmd` 端到端测**，而在流式打印架构下它几乎免费。

**出局（Q21）**：

skills、slash commands、prompt templates、themes、compaction、session branching / fork / clone、图片输入、`@` 文件引用、MCP client、子 agent、OAuth 登录、token 本地精确计算、OS 沙箱、JS/WASM 插件运行时。

post-v0 第一项是 **MCP client**（Q7 扩展故事的落地），但要在 dogfood 过一轮、知道自己真缺什么之后再做。

**Q16 token 计算**：只显示 provider 返回的 `usage`（权威且免费），chars/4 粗估仅用于"快要爆窗口"的事前警告。**不引 `tiktoken-rs`**：它带 BPE 表吃体积预算，而且对 DeepSeek / Kimi / Claude 本来就是错的——付了体积代价仍然只拿到估计值。

---

## 12. 实现顺序

排序原则：**先建立那些一旦错了就要重写的不变量，再建立那些能让你看到 token 滚动的部分。**

**M0 环境**
workspace 骨架、edition 2024、release profile、CI（`fmt` + `clippy -D warnings` + `test`，体积断言先留 TODO）。
*注：本机 cargo 需走代理访问 `index.crates.io` / `static.crates.io`，已验证可用。*

**M1 脊椎（零网络、零终端）**
领域类型（`Message` / `ContentBlock` / `ToolCall` / `ToolResult` / `SessionEntry{id, parent_id}`）→ SSE 状态机 + fixture → session 写入与 `--continue` 重建 → MockProvider → loop 与 abort 语义。

完成标志：`cargo test` 证明"多轮 + tool 调用 + 工具报错 + abort 后状态可恢复"全部成立。这一步没有任何可看的东西，但它决定项目成败。

**M2 工具层**
`read` / `write` / `glob` / `grep` 先做（简单），剩余时间全砸给 `edit`（见第 5 节测试矩阵）。最后 `bash`（`kill_on_drop` / 超时 / 输出截断）。

**M3 真 provider**
reqwest + rustls + SSE 接到 M1 的状态机上，retry 策略，wiremock 传输层测试。这一步结束第一次看到真实模型在动。

**M4 CLI**
`--print` 先落（可被 `assert_cmd` 端到端测）→ reedline + 流式打印 → 权限确认 UI → `AGENTS.md` 加载。

**M5 dogfood**
用 crab 改 crab，每次失败记成 issue。

**已知张力**：M1 要求在看到任何真实 token 之前先写三四天纯内部代码，对单人项目是士气风险。允许第一周写一个 50 行、直连 API、只为把字节打印出来的 spike，**看完即删**——不要让它长成 provider 实现，否则 loop 会被第一家 API 的形状拧歪。

---

## 13. 三个最可能杀死项目的东西

**一、不是 Rust 的问题，是 prompt 的问题。**
agent 的实际质量住在 system prompt 和工具描述里。如果 crab 用起来比 pi 笨，没有任何性能数字能救它，而且你会误以为是代码有 bug。
缓解：固定一组 5-10 个真实任务，每次改 prompt 都跑一遍肉眼比。**从 M4 就开始，不要等到 M5。**

**二、Q2 有鸡生蛋问题。**
`edit` 不够好之前，crab 写不了 crab。
缓解：门槛分级。第一次 crab 产出的 commit 允许是"改个错别字 + 加一个测试"这种量级，不要一上来就要它实现 M3。

**三、第 11 节的出局清单会反复敲门。**
每一项都有"就一小会儿"的说服力。
唯一有效的防线：**在你自己能用 crab 干活之前，不加任何围栏外的功能。**

---

## 附录：核实过的版本（查询时点）

| crate | 版本 | 用途 |
| --- | --- | --- |
| tokio | 1.53.2 | 异步运行时 |
| reqwest | 0.13.5 | HTTP（注意 feature 改名） |
| eventsource-stream | 0.2.3 | SSE |
| crossterm | 0.29.0 | 终端 |
| reedline | 0.52.1 | 输入行 |
| ratatui | 0.30.2 | 后续 `Viewport::Inline` |
| similar | 3.2.0 | 仅用于 edit 报错提示 |
| rmcp | 3.5.1 | post-v0 MCP client |
| insta | 1.49.0 | 快照测试 |
| wiremock | 0.6.5 | 传输层测试 |
| assert_cmd | 2.2.2 | CLI 测试 |
| tempfile | 3.27.0 | 测试临时目录 |

**已评估并排除**：`rig-core` 0.44.0（拿走 loop）、`rquickjs` 0.14.0 / `wasmtime` 49.0.2（插件运行时）、`tiktoken-rs` 0.12.1（体积 + 对非 OpenAI 模型不准）。
