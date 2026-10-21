# ContextMeter 实施计划（resume 时 seed context 占用）

Spec: `docs/superpowers/specs/2026-10-21-context-meter-design.md`（2026-10-21）

核心改动：`crab-core` 新增 `meter.rs`（纯函数 replay 测量），`--continue` 时
`build_agent` 折叠会话日志把 `context_used` 塞进 `Startup`，TUI 启动即用真实
占用渲染 CH 段，不再空白到第一次 LLM 调用。

## Task 1: crab-core — meter.rs

- [x] **Step 1: 模块 + 函数 + 单测**。`crates/crab-core/src/meter.rs`：
  `pub fn context_used(entries: &[SessionEntry]) -> Option<u64>`（反向 find 最近一次
  `ObsBody::LlmCall { usage: Some(u) }` 的 `u.prompt`）；`lib.rs` 加 `pub mod meter;`。
  测试（全在 meter.rs 内）：
  - 两次带 usage 的 LlmCall（不同 prompt）→ 返回最后一次的 prompt
  - 尾部 LlmCall usage=None → 跳过取更早的；全 None → None
  - 只有 Meta + user/assistant 消息（v1 形态）→ None
  - 空 entries → None
  - `Session::create(tempdir)` + `append_obs` ×2 真实往返 → 与手写 entries fold 一致
- [x] **Step 2: 跑绿** `cargo test -p crab-core meter`

## Task 2: app — Startup 携带 context_used

- [x] **Step 1: `Startup` 加字段** `pub context_used: Option<u64>`（app.rs）。
- [x] **Step 2: `build_agent` 折叠**：`Agent::new` 消费 session 之前
  `let context_used = if continue_ { meter::context_used(session.entries()) } else { None };`，
  存入 Startup。`Startup` 构造的测试同步补字段。
- [x] **Step 3: 测试**：合成 session 文件（Meta + 带 usage 的 obs 行）→
  `build_agent` 重 IO/依赖多，不测整条；改测轻点——meter 往返已在 Task 1 覆盖，
  这里只保证 `if continue_` 分支编译走到（依赖类型系统）。不新增测试，标注原因。
- [x] **Step 4: 跑绿** `cargo test -p crab-cli app`

## Task 3: TUI — 启动 seed CH 段

- [x] **Step 1: `TuiState::new` 签名**加第 4 参 `context_used: Option<u64>`，
  `set_context(None, window)` → `set_context(context_used, window)`。
  11 个测试调用点（state.rs / mod.rs tests / view.rs）机械补 `None`。
- [x] **Step 2: 真实接线**：`tui/mod.rs` 构造处传 `startup.context_used`
  （`build_agent` 返回后 destructure，已有先例）。
- [x] **Step 3: 测试**：`TuiState::new(..., Some(85_000))` + window 128_000
  → 首帧 `format_status_row` 即含 `CH66%`（不依赖任何 Usage 事件）。
- [x] **Step 4: 跑绿** `cargo test -p crab-cli tui`

## Task 4: 收尾

- [x] `cargo test --workspace` 全绿；`cargo clippy --all-targets -- -D warnings` 干净
- [x] e2e 冒烟：mock provider 跑一轮 `--print`（新建会话，CH 逻辑不炸）；
  再用 `--continue` 起 TUI 手看一眼 CH 段出现（人验）
- [x] Commit `feat(core): context occupancy seeded at resume — deterministic replay of the session log`

## Self-Review

- **类型一致**：`context_used: Option<u64>`（Task 2 Startup 定义 → Task 3 TuiState 消费）；
  `meter::context_used(&[SessionEntry]) -> Option<u64>`（Task 1 定义，Task 2 调用）。
- **Spec 覆盖**：反向 find（Task 1）；v1/空/无 usage → None（Task 1 测试表）；
  启动 seed（Task 2+3）；CH 段首帧（Task 3 测试）。非目标均未进计划。✅
- **注意点**：① `Agent::new` 按值消费 session，fold 必须在它之前；② 12 个
  `TuiState::new` 调用点一个不能漏（编译器兜底，但 clippy -D warnings 下漏传 None
  是编译错不是警告）；③ 运行中路径零改动——Usage 事件照旧，seed 只发生在启动。
