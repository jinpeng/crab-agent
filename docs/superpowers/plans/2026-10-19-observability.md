# 可观测性（一期：Log + Trace + Metrics）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** session JSONL v2（meta 行 + obs 事件 + usage 四分）→ `crab trace` span 树 → SQLite 聚合 + `crab metrics` 报告。Evaluation 二期，本计划不含。

**Architecture:** 日志即真相——obs 与 message 同文件 append、严格分离，历史重建只读 message；在线只记事实（timing/usage/digest），trace 与聚合都是离线 replay；metrics sink 是 broadcast 的独立接收器（观测不侵入现有 forwarder）；`blake3` 进 core，`rusqlite`/`toml` 进 cli（core 红线不破）。

**Spec:** `docs/superpowers/specs/2026-10-19-observability-design.md`

**前置：** 无。工作分支从 main 切。cargo 需代理（`export https_proxy=http://127.0.0.1:10808`）。

---

### Task 1: core — session v2 格式 + obs 事件类型

**Files:**
- Modify: `crates/crab-core/src/session.rs`
- Modify: `crates/crab-core/src/agent.rs`（`Agent::new` 历史重建过滤，变体新增后的编译修复）
- Create: `crates/crab-core/src/obs.rs`
- Modify: `crates/crab-core/src/lib.rs`（`pub mod obs;`）
- Modify: `crates/crab-core/Cargo.toml`（+ `blake3`）
- Modify: `Cargo.toml`（workspace deps 加 `blake3 = "1"`、`rusqlite = { version = "3", features = ["bundled"] }`，Task 5 用 rusqlite）

- [ ] **Step 1: 失败测试**——`crates/crab-core/src/obs.rs` 先建类型 + 测试：

```rust
//! Observability events (obs) — measurement-only facts appended to the session
//! log. NEVER part of the model-visible history (see spec: 日志即真相).

use serde::{Deserialize, Serialize};

/// Usage reported by the provider, four-way split (DSH tokenUsage shape).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmUsage {
    pub prompt: u32,
    pub completion: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ObsOutcome {
    Ok,
    /// Truncated to 128 chars; full detail lives in the message log.
    Error(String),
    Aborted,
}

/// One observability record. `run_id` identifies one `Agent::run` call (one
/// user message = one trace).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObsEvent {
    pub run_id: u64,
    pub ts: u64, // unix epoch millis
    #[serde(flatten)]
    pub body: ObsBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "obs", rename_all = "snake_case")]
pub enum ObsBody {
    TurnStart,
    TurnEnd {
        stop_reason: crate::types::StopReason,
        interrupted: bool,
        llm_calls: u32,
        tool_calls: u32,
    },
    LlmCall {
        provider: String,
        model: String,
        duration_ms: u64,
        usage: Option<LlmUsage>,
        outcome: ObsOutcome,
    },
    ToolCall {
        tool: String,
        call_seq: u32,
        args_digest: String,
        result_digest: String,
        bytes_in: u64,
        bytes_out: u64,
        duration_ms: u64,
        outcome: ObsOutcome,
    },
}

/// Canonical-json digest (blake3, first 8 hex chars): `{"a":1,"b":2}` sorts
/// keys so logically-equal args hash equal regardless of map order.
pub fn digest(v: &serde_json::Value) -> String {
    let canonical = crate::util::canonical_json(v);
    let mut h = blake3::Hasher::new();
    h.update(canonical.as_bytes());
    h.finalize().to_hex()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_roundtrips_with_cache_fields() {
        let u = LlmUsage { prompt: 10, completion: 5, cache_read: 3, cache_write: 7 };
        let s = serde_json::to_string(&u).unwrap();
        assert_eq!(serde_json::from_str::<LlmUsage>(&s).unwrap(), u);
    }

    #[test]
    fn digest_is_key_order_independent() {
        let a = serde_json::json!({"path": "x", "limit": 1});
        let b = serde_json::json!({"limit": 1, "path": "x"});
        assert_eq!(digest(&a), digest(&b));
        assert_ne!(digest(&a), digest(&serde_json::json!({"path": "y", "limit": 1})));
        assert_eq!(digest(&a).len(), 8);
    }
}
```

`crates/crab-core/src/util.rs` 新增 `canonical_json`（递归排序对象键；数组保持顺序；测试覆盖嵌套）——若 util 模块不存在则新建并在 lib.rs 注册。

- [ ] **Step 2: session.rs 改造**（先跑红：`cargo test -p crab-core session` 编译 FAIL——变体不匹配）

```rust
/// 当前 writer 格式版本；唯一手工维护的事实源（DSH 式纪律）。
pub const SESSION_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    Message { id: EntryId, parent_id: Option<EntryId>, message: Message },
    Meta { format_version: u32, created: String },
    Obs { id: EntryId, event: crate::obs::ObsEvent },
}
```

- `Session` 加字段 `format_version: u32`；`create()` 先写 `Meta { format_version: SESSION_FORMAT_VERSION, created: rfc3339-now }` 再返回；`load()` 逐行解析，首行是 Meta 取其版本，否则按 1 处理（无 meta 行的既有文件）；坏行沿用现有"尽力读"策略。
- `append_obs(&mut self, event: ObsEvent) -> io::Result<EntryId>`：同 `append`（顺序 id，parent_id=None）。
- `messages(&self) -> impl Iterator<Item = &Message>`：只出 Message 变体。
- `SessionEntry::id()`：Meta → `u64::MAX`（不参与 parent 链；或给 Meta/Obs 也顺序编号——选**顺序编号**，与 append 一致，parent_id 仅 message 使用）。
- `Agent::new` 历史重建改为 `session.messages().cloned().collect()`。

- [ ] **Step 3: 测试**（session.rs tests 追加）

```rust
    #[test]
    fn v2_writes_meta_first_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("hi")).unwrap();
        s.append_obs(crate::obs::ObsEvent { run_id: 0, ts: 1, body: crate::obs::ObsBody::TurnStart }).unwrap();
        let text = std::fs::read_to_string(s.path()).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["kind"], "meta");
        assert_eq!(first["format_version"], 2);

        let back = Session::load(s.path()).unwrap();
        assert_eq!(back.format_version(), 2);
        assert_eq!(back.messages().count(), 1);
        assert!(matches!(&back.entries()[2], SessionEntry::Obs { .. }));
    }

    #[test]
    fn v1_file_without_meta_loads_as_version_1() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        std::fs::write(&p, "{\"kind\":\"message\",\"id\":0,\"parent_id\":null,\"message\":{\"role\":\"user\",\"content\":[]}}\n").unwrap();
        let s = Session::load(&p).unwrap();
        assert_eq!(s.format_version(), 1);
        assert_eq!(s.messages().count(), 1);
    }
```

（既有损坏行容忍测试保持不变。）

- [ ] **Step 4: 跑绿**——`cargo test -p crab-core 2>&1 | grep 'test result'`。

- [ ] **Step 5: Commit**

```bash
git add crates/crab-core/src/session.rs crates/crab-core/src/obs.rs crates/crab-core/src/util.rs crates/crab-core/src/lib.rs crates/crab-core/Cargo.toml Cargo.toml Cargo.lock
git commit -m "feat(core): session JSONL v2 — meta row, obs events, canonical-json digests"
```

---

### Task 2: core — usage 四分透传

**Files:**
- Modify: `crates/crab-core/src/types.rs`（`AssistantTurn.usage: Option<crate::obs::LlmUsage>`）
- Modify: `crates/crab-core/src/stream.rs`（`StreamEvent::Usage` 四字段 + 累加器）
- Modify: `crates/crab-core/src/provider/openai.rs`（SSE usage 解析 cache 字段）
- Modify: `crates/crab-core/src/agent.rs`（`AgentEvent::Usage` 四字段 + bridge）
- Modify: `crates/crab-cli/src/tui/state.rs`（Usage 处理适配）

- [ ] **Step 1: 失败测试**——openai.rs tests 的 usage fixture 扩展。`stream.rs` 的 accumulator 测试（`usage_event_is_captured_into_turn`）改为四字段断言：

```rust
    #[test]
    fn usage_event_with_cache_is_captured_into_turn() {
        // StreamEvent::Usage { prompt: 10, completion: 5, cache_read: 3, cache_write: 2 }
        // ...accumulate...assert_eq!(t.usage, Some(LlmUsage { prompt: 10, completion: 5, cache_read: 3, cache_write: 2 }));
    }

    #[test]
    fn usage_without_cache_fields_defaults_zero() {
        // SSE usage chunk 只有 prompt_tokens/completion_tokens 时
        // cache_read/cache_write == 0
    }
```

openai.rs 新增 fixture：DeepSeek usage chunk 含 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens` → 映射 `cache_read=hit, cache_write=miss`；无 cache 字段 → 0。

- [ ] **Step 2: 改类型与解析**

- `StreamEvent::Usage { prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens }`（stream.rs:20 附近）。
- stream.rs 累加器（`usage: Option<(u32,u32)>` → `Option<LlmUsage>`，:80、:131 处）。
- openai.rs：SSE `usage` chunk 解析处（:80 附近）读 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`（`Option<u32>`，缺省 0）；:42 字段类型同上。
- `AssistantTurn.usage: Option<crate::obs::LlmUsage>`（types.rs:53）。
- `AgentEvent::Usage { prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens }`（agent.rs:20 附近）；forward bridge 的 `StreamEvent::Usage` 映射同步。
- `state.rs` Usage 分支：`self.status.add_usage(prompt_tokens, completion_tokens);`（cache 暂不进状态栏，v1 保持）。

- [ ] **Step 3: 跑绿**——`cargo test --workspace 2>&1 | grep -E 'test result|FAILED'`。

- [ ] **Step 4: Commit**

```bash
git add crates/crab-core/src/types.rs crates/crab-core/src/stream.rs crates/crab-core/src/provider/openai.rs crates/crab-core/src/agent.rs crates/crab-cli/src/tui/state.rs
git commit -m "feat(core): four-way usage (prompt/completion/cache read+write) end to end"
```

---

### Task 3: core — agent loop 发射 obs 事件

**Files:**
- Modify: `crates/crab-core/src/agent.rs`

- [ ] **Step 1: 失败测试**——agent.rs tests 用 `MockProvider` + `Noop` 工具（既有测试基建）：

```rust
    #[tokio::test]
    async fn run_writes_obs_turn_llm_tool_events() {
        // MockProvider 一次 ToolUse 响应（Noop 工具）一次 EndTurn。
        // 断言 session entries 里 obs 序列：
        // TurnStart(run 0) → LlmCall(outcome Ok, usage Some) → ToolCall(tool="noop",
        // call_seq 0, args_digest == digest(args), bytes_out == len("done")) →
        // 第二次 LlmCall → TurnEnd{stop_reason: EndTurn, interrupted: false,
        // llm_calls: 2, tool_calls: 1}
    }

    #[tokio::test]
    async fn aborted_run_writes_interrupted_turn_end() {
        // abort_token().cancel() 后 run → TurnEnd{interrupted: true}
    }

    #[tokio::test]
    async fn provider_error_still_writes_turn_end() {
        // MockProvider error 响应 → TurnEnd 存在（TurnStart 之后），运行返回 Err
    }
```

MockProvider 用法参照既有测试（`MockProvider::new(vec![vec![StreamEvent...]])`）。

- [ ] **Step 2: 实现**

- `Agent` 加 `next_run_id: u64`（`Agent::new` 初始 0）。
- `run()` 里包一层：`let run_id = self.next_run_id; self.next_run_id += 1;` 传入 `run_inner`；无论 `run_inner` 结果如何，退出路径都写 `TurnEnd`——结构改为 `run_inner` 返回前统一收尾：

```rust
    async fn run_inner(&mut self, user_input: String, run_id: u64, stream_tx: ...) -> Result<StopReason, AgentError> {
        let mut llm_calls = 0u32;
        let mut tool_calls = 0u32;
        self.obs(run_id, ObsBody::TurnStart);   // obs() = append_obs + eprintln warn on failure
        let started = std::time::Instant::now();
        let result = self.turn_loop(user_input, run_id, &mut llm_calls, &mut tool_calls, stream_tx).await;
        let (stop_reason, interrupted, outcome_note) = match &result {
            Ok(r) => (*r, matches!(r, StopReason::Aborted), None),
            Err(e) => (StopReason::Aborted, true, Some(e.to_string())),
        };
        self.obs(run_id, ObsBody::TurnEnd {
            stop_reason,
            interrupted,
            llm_calls,
            tool_calls,
        });
        // outcome_note 仅用于未来扩展；错误详情已在 message log / 返回值中
        result
    }
```

（把现有 `for turn in 0..` 循环体抽到 `turn_loop`；循环内：）
- 每次 `provider.complete` 前 `let t0 = Instant::now();`；成功后 `llm_calls += 1` 并 `self.obs(run_id, ObsBody::LlmCall { provider: self.provider_label(), model: ..., duration_ms: t0.elapsed().as_millis() as u64, usage: t.usage, outcome: Ok })`。provider/model 标签：agent 目前不存——`AgentConfig` 加 `label: String`？**简化**：`provider.name()` 若无则加 `fn label(&self) -> &str` 到 Provider trait（mock/openai 实现："mock"/"openai-compatible"），model 从 openai 的 model 字段——Provider trait 加 `fn model(&self) -> &str`。评估改动面小，执行。
- 工具执行处：计时、`bytes_in = input.to_string().len()`、`bytes_out = content.len()`、`tool_calls += 1`、emit `ToolCall { tool: name.clone(), call_seq: tool_calls - 1, args_digest: digest(input), result_digest: digest(&serde_json::Value::String(content.clone())), ...outcome: Ok | Error(e truncated 128) }`。
- 现有 `AgentEvent` 发射保持不变（观测不侵入现有事件流；Task 5 的 metrics sink 直接订阅现有 AgentEvent + 计时，无需新事件——见 Task 5）。

- [ ] **Step 3: 跑绿**——`cargo test -p crab-core agent 2>&1 | grep 'test result'`。

- [ ] **Step 4: Commit**

```bash
git add crates/crab-core/src/agent.rs crates/crab-core/src/provider/mod.rs crates/crab-core/src/provider/openai.rs
git commit -m "feat(core): agent loop emits obs turn/llm/tool events into the session log"
```

---

### Task 4: cli — `crab trace` 子命令

**Files:**
- Modify: `crates/crab-cli/src/main.rs`（clap subcommand 路由）
- Create: `crates/crab-cli/src/trace.rs`

- [ ] **Step 1: clap 子命令**（main.rs）：

```rust
#[derive(clap::Subcommand)]
enum Commands {
    /// Render the span tree of a session file (turn → llm/tool calls).
    Trace { file: std::path::PathBuf, #[arg(long)] json: bool },
    /// Token/cost/latency report from the local metrics database.
    Metrics { #[arg(default_value = "7")] days: u32, #[arg(long)] json: bool },
}

#[derive(Parser)]
#[command(name = "crab", version, about = "Rust coding agent")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    // 现有字段不变
}
```

`main()`：`match cli.command { Some(Commands::Trace{..}) => return trace::run(...), Some(Commands::Metrics{..}) => ...（Task 5）, None => 现有路径 }`。

- [ ] **Step 2: 失败测试**——trace.rs 先写测试（纯逻辑 seams）：

```rust
// Span 模型
pub struct SpanNode { pub label: String, pub detail: String, pub duration_ms: Option<u64>, pub tokens: Option<LlmUsage>, pub outcome: String }
pub struct Trace { pub run_id: u64, pub spans: Vec<SpanNode>, pub complete: bool }  // complete=false → 渲染标 incomplete

/// 从 session entries 构建 traces：obs 事件流式分桶到 run；tool_call 与 message 中
/// 第 call_seq 个 ToolUse join（digest 校验），取 name/args 摘要进 detail。
pub fn build_traces(entries: &[SessionEntry]) -> Vec<Trace>

/// 文本树渲染（宽度截断用 crate::width::truncate_to_width）。
pub fn render(traces: &[Trace]) -> String
```

测试：
- fixture 构造 `SessionEntry` 序列（v2：meta + message ToolUse + obs 交错）→ `build_traces` 断言 span 数、顺序、digest join 后 detail 含工具名；
- digest 不匹配 → span detail 标 `join-mismatch`；
- 有 TurnStart 无 TurnEnd → `complete == false`；
- 纯 v1（只有 message）→ 单 trace、span 只有 message 级摘要（无 obs 也渲染，退化为对话轮廓）。

- [ ] **Step 3: 实现** trace.rs（`run(file, json)`：load session → build_traces → render/serde_json 输出）。

- [ ] **Step 4: 跑绿 + Commit**

```bash
cargo test -p crab-cli trace 2>&1 | grep 'test result'
git add crates/crab-cli/src/main.rs crates/crab-cli/src/trace.rs
git commit -m "feat(cli): crab trace renders the span tree of a session file"
```

---

### Task 5: cli — metrics sink + `crab metrics`

**Files:**
- Modify: `crates/crab-cli/Cargo.toml`（+ `rusqlite`、`toml`）
- Create: `crates/crab-cli/src/metrics.rs`、`crates/crab-cli/pricing.toml`
- Modify: `crates/crab-cli/src/main.rs`（wiring + Metrics 子命令）
- Modify: `crates/crab-cli/src/tui/mod.rs`（TUI 路径 wiring）

- [ ] **Step 1: 失败测试**——metrics.rs：

```rust
/// 消费 AgentEvent 累积一个 run 的事实，Finished 时产出 turns 行。
pub struct RunAccumulator { /* turn 开始时间、累计 usage、调用计数、错误计数、model */ }

impl RunAccumulator {
    pub fn new(pricing: Pricing) -> Self;
    pub fn on_event(&mut self, ev: &AgentEvent);   // 内部计时
    pub fn take_turn(&mut self, session_id: &str, run_id: u64) -> Option<TurnRow>; // Finished 后调用
}

pub struct Pricing { /* HashMap<model, Rates> */ }
pub fn parse_pricing(toml_str: &str) -> Pricing;  // [models.<name>] input/output/cache_read/cache_write
pub fn cost_usd(p: &Pricing, model: &str, u: &LlmUsage) -> Option<f64>;
```

测试：pricing 解析（含缺字段模型 → None cost）；cost 计算（cache 价命中、未知模型 None）；accumulator 喂事件序列（TurnStarted → Usage×2 → ToolCallStarted/Finished(is_error=true) → Finished）→ take_turn 行的计数/usage/错误/duration 区间。

- [ ] **Step 2: 实现**

- `pricing.toml`（workspace 根 pricing，单位 USD/1M tokens，2026-10 市价，注释标明日期与来源）：

```toml
[models.deepseek-chat]
input = 0.27
output = 1.10
cache_read = 0.07
cache_write = 0.07

[models.deepseek-reasoner]
input = 0.55
output = 2.19
cache_read = 0.14
cache_write = 0.14
```

（cli 里 `include_str!("../pricing.toml")` 编译期嵌入。）

- SQLite（rusqlite bundled）：

```rust
pub struct MetricsDb { conn: rusqlite::Connection }
impl MetricsDb {
    pub fn open(path: &Path) -> Result<Self>;      // WAL + 建表（spec schema）+ 幂等 seed pricing 不需要（pricing 在内存）
    pub fn record_turn(&self, r: &TurnRow) -> Result<()>;
    pub fn daily_report(&self, days: u32) -> Result<Vec<DailyRow>>;   // 按日聚合
    pub fn model_report(&self, days: u32) -> Result<Vec<ModelRow>>;   // 按模型聚合
}
```

text 报告渲染（含 p50/p90 duration、tool 错误率、cost 为 NULL 显示 `-`）+ `--json`。

- [ ] **Step 3: wiring**——broadcast 天然多接收器，sink 独立订阅：

main.rs `run_print` 与 tui/mod.rs `run()` 在 agent 创建后各加：

```rust
    // Best-effort metrics sink: an independent broadcast receiver; a failure
    // here never affects the run (观测不改变系统).
    let mut sink = crate::metrics::MetricsSink::open(&cwd);
    if let Ok(sink) = sink.take() {
        let mut rx = agent.subscribe();
        tokio::spawn(async move {
            let mut acc = RunAccumulator::new(Pricing::embedded());
            while let Ok(ev) = rx.recv().await {
                if let Some(row) = acc.on_event(&ev) {
                    let _ = sink.record_turn(&row);  // warn on failure
                }
            }
        });
    }
```

`on_event` 返回 `Option<TurnRow>`（Finished 时 Some）。session_id：`agent.session_entries()` 首行的 session 文件名——`Agent` 加 `pub fn session_id(&self) -> String`（path 文件名 stem）。

- [ ] **Step 4: 跑绿 + clippy + Commit**

```bash
cargo test --workspace 2>&1 | grep -E 'test result|FAILED'
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
git add crates/crab-cli/src/metrics.rs crates/crab-cli/pricing.toml crates/crab-cli/Cargo.toml crates/crab-cli/src/main.rs crates/crab-cli/src/tui/mod.rs Cargo.lock crates/crab-core/src/agent.rs
git commit -m "feat(cli): metrics sink + crab metrics report (turns, tokens, cost, latency)"
```

---

### Task 6: 收尾验证

- [ ] **Step 1: 全套件 + 红线 + 体积**

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep -cE 'test result: ok'   # 6
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap|rusqlite' && echo VIOLATION || echo "core clean"
cargo build --release && ls -l target/release/crab | awk '{printf "%.2f MiB\n", $5/1048576}'
```

- [ ] **Step 2: e2e smoke**——真实 TUI 跑一轮对话（pty 脚本）→ `crab trace` 看 span 树、`crab metrics` 看聚合、`grep -c '"kind":"obs"' .crab/sessions/<最新>` 确认 obs 落盘。记录结果。

- [ ] **Step 3: Commit（如有修正）+ 更新 DESIGN.md？**

不改 DESIGN.md（观测是横切能力，非架构决策）；如需记录，在 dogfood-log 补一条人驱会话条目（可选）。

---

## Self-Review

- **Spec 覆盖**：meta/version（Task 1）、obs vocab（Task 1+3）、usage 四分（Task 2）、digest/join（Task 1 digest + Task 4 join）、trace 渲染含 incomplete/mismatch（Task 4）、metrics schema/cost/pricing/报告（Task 5）、错误处理表（Task 1 坏行容忍、Task 3 obs 失败 warn、Task 5 sink best-effort；孤儿 turn 无 turn_end → metrics 自然无该行 = spec"不计该 turn"）、红线测试（Task 1 `messages()` 过滤 + agent 历史重建只用它）。✅
- **占位符**：无；关键代码均给出。✅
- **类型一致**：`LlmUsage`（Task 1 定义，Task 2/3/5 复用）、`ObsEvent{run_id,ts,body}`（Task 1 定义，Task 3 发射、Task 4 消费）、`digest()`（Task 1，Task 3 用）、`RunAccumulator::on_event -> Option<TurnRow>`（Task 5 定义并接线）、`MetricsSink::open/record_turn`（Task 5）。`SessionEntry::Meta/Obs` 使既有 `Agent::new` 的穷尽 match 编译失败——Task 1 Step 2 一并修复（messages()）。✅
- **坑**：① rusqlite bundled 首次编译慢，留出时间；② `AgentEvent::Usage` 改四字段会同时打破 state.rs 与 TUI 测试——Task 2 一并改；③ broadcast 接收器必须在 `agent` drop 前建立；④ obs append 在 agent loop 内是同步文件写（每次一行，微秒级，可接受）；⑤ `include_str!("../pricing.toml")` 路径相对 cli crate 根。
