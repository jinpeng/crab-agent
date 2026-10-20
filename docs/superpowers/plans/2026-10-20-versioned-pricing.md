# 定价 schema v2 实现计划（多版本 + 峰谷 + 历史价）

> **For agentic workers:** 逐步执行，checkbox 跟踪。
> 前置：`main` @ 0892779。设计已批准（用户对话 2026-10-20）。

**Goal:** pricing 从"按模型名查表"变为"按调用时刻解析"：版本链（since 生效时间）+ 峰谷窗口（tz 本地时间，支持跨夜）；成本细化到每次 Usage 事件。

**Architecture:** 全部在 `crates/crab-cli/src/metrics.rs`（+ 根 `pricing.toml` 新 schema）。手写精简 RFC3339 解析与 civil 历算（无 chrono 依赖）。`TurnRow` 与渲染不变（`cost_usd: Option<f64>`，None → `-`）。

**非目标:** 自动抓取官方价格；为内置表编造 since 日期（标 TODO 由人核实）。

---

### Task 1: 时间基元（解析 + 窗口匹配）

**Files:** Modify `crates/crab-cli/src/metrics.rs`

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn rfc3339_parses_known_values() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2024-01-01T00:00:00Z"), Some(19_723 * 86_400_000));
    // +08:00 时区：本地 2024-01-01 00:00 = UTC 2023-12-31 16:00
    assert_eq!(
        parse_rfc3339("2024-01-01T00:00:00+08:00"),
        Some(19_723 * 86_400_000 - 8 * 3_600_000)
    );
    assert_eq!(parse_rfc3339("2024-01-01T00:00:00-05:30"), Some(19_723 * 86_400_000 + 5 * 3_600_000 + 30 * 60_000));
    assert!(parse_rfc3339("not-a-date").is_none());
    assert!(parse_rfc3339("2024-01-01").is_none());
}

#[test]
fn window_matching_includes_overnight() {
    // 谷 00:30-08:30（+08:00）
    let w = Window { start_min: 30, end_min: 8 * 60 + 30, tz_min: 8 * 60, rates: Rates::default() };
    assert!(w.matches(6 * 60));        // 06:00 本地
    assert!(!w.matches(12 * 60));      // 12:00 本地
    // 跨夜 16:30-00:30
    let ovn = Window { start_min: 16 * 60 + 30, end_min: 30, tz_min: 8 * 60, rates: Rates::default() };
    assert!(ovn.matches(20 * 60));
    assert!(ovn.matches(0));           // 00:00 属于前一日窗口
    assert!(!ovn.matches(8 * 60));
}
```

- [ ] **Step 2: 实现**

```rust
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 { /* Hinnant：era/yoe/mp/doy/doe，测试锁 1970-01-01=0 */ }

/// RFC3339 子集：YYYY-MM-DDTHH:MM:SS(Z|±HH:MM)。返回 epoch millis。
fn parse_rfc3339(s: &str) -> Option<i64> { /* 定长切片 + 校验分隔符；非法返回 None */ }

/// `+08:00` / `-05:30` / `Z` → 分钟；非法 None。
fn parse_tz(s: &str) -> Option<i32> { }

fn parse_hhmm(s: &str) -> Option<i32> { /* "00:30" → 30 分钟 */ }

struct Window { start_min: i32, end_min: i32, tz_min: i32, rates: Rates }
impl Window {
    fn matches(&self, ts_ms: i64) -> bool {
        let local = (((ts_ms / 60000) + self.tz_min as i64).rem_euclid(1440)) as i32;
        if self.start_min < self.end_min {
            local >= self.start_min && local < self.end_min
        } else if self.start_min > self.end_min {
            local >= self.start_min || local < self.end_min
        } else {
            false // 空窗口不匹配
        }
    }
}
```

- [ ] **Step 3: 跑绿 + Commit** `feat(metrics): time primitives for versioned pricing (rfc3339, windows)`

### Task 2: Pricing schema v2 + 解析/合并

- [ ] **Step 1: 失败测试**

```rust
#[test]
fn flat_entries_become_single_epoch_version() {
    let p = parse_pricing("[models.x]\ninput = 1.0\noutput = 2.0\n");
    let v = &p.models["x"];
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].since, 0);
    assert_eq!(v[0].base.output, 2.0);
}

#[test]
fn versioned_entries_sort_and_carry_windows() {
    let toml = r#"
[models.dc]
[[models.dc.versions]]
since = "2025-09-06T16:30:00+08:00"
input = 0.99
output = 2.99
[[models.dc.versions.windows]]
start = "00:30"
end = "08:30"
tz = "+08:00"
input = 0.5
[[models.dc.versions]]
since = "2024-12-01T00:00:00+08:00"
input = 0.27
output = 1.10
"#;
    let p = parse_pricing(toml);
    let v = &p.models["dc"];
    assert_eq!(v.len(), 2);
    assert!(v[0].since < v[1].since, "升序");
    assert_eq!(v[1].windows.len(), 1);
}

#[test]
fn cost_resolves_by_timestamp_and_peak_valley() {
    let p = parse_pricing(/* 上例 toml */);
    let u = LlmUsage { prompt: 1_000_000, ..Default::default() };
    let t_old = parse_rfc3339("2025-01-01T12:00:00+08:00").unwrap();
    let t_peak = parse_rfc3339("2025-10-01T12:00:00+08:00").unwrap(); // 峰
    let t_valley = parse_rfc3339("2025-10-01T04:00:00+08:00").unwrap(); // 谷
    assert!((cost_usd(&p, "dc", t_old, &u).unwrap() - 0.27).abs() < 1e-9);
    assert!((cost_usd(&p, "dc", t_peak, &u).unwrap() - 0.99).abs() < 1e-9);
    assert!((cost_usd(&p, "dc", t_valley, &u).unwrap() - 0.5).abs() < 1e-9);
    // 早于首条 since / 未知模型 → None
    assert!(cost_usd(&p, "dc", 0, &u).is_none());
    assert!(cost_usd(&p, "nope", t_peak, &u).is_none());
}
```

- [ ] **Step 2: 实现**

```rust
pub struct Version { since: i64, base: Rates, windows: Vec<Window> }
pub struct Pricing { pub models: HashMap<String, Vec<Version>> }  // versions 按 since 升序

pub fn parse_pricing(toml_str: &str) -> Pricing {
    // 手走 toml::Value：models 表下每个模型，有 "versions" 数组 → 版本链；
    // 否则 → 单版本 since=0（向后兼容扁平格式）。since/window 解析失败 eprintln warn 并跳过该项。
}

pub fn cost_usd(p: &Pricing, model: &str, ts_ms: i64, u: &LlmUsage) -> Option<f64> {
    let versions = p.models.get(model)?;
    let v = versions.iter().rev().find(|v| v.since <= ts_ms)?;
    let rates = v.windows.iter().find(|w| w.matches(ts_ms)).map(|w| &w.rates).unwrap_or(&v.base);
    Some((u.prompt as f64 * rates.input + u.completion as f64 * rates.output
        + u.cache_read as f64 * rates.cache_read + u.cache_write as f64 * rates.cache_write) / 1e6)
}
```

`load_pricing_from` 合并改为：同模型 versions 拼接后按 since 排序；**同 since**：字段非零覆盖、`windows` 用户非空则替换。

- [ ] **Step 3: 跑绿 + Commit** `feat(metrics): versioned pricing with peak/valley windows`

### Task 3: 内置 pricing.toml 迁移 + per-Usage 成本

- [ ] **Step 1: 迁移根 `crates/crab-cli/pricing.toml`** 到版本链 schema（数值不变，`since = "1970-01-01T00:00:00Z"` 占位 + 注释 `TODO: verify real effective dates`，附峰谷窗口写法示例注释）。
- [ ] **Step 2: accumulator 改 per-Usage**：`Usage` 分支用**收到事件的 `now_ms()`** 调 `cost_usd`；`self.cost: Option<f64>` 起始 `Some(0.0)`，未定价事件置 `None`（粘性）；`usage_events` 计数；`Finished` 时 `usage_events == 0 → cost = None`。
- [ ] **Step 3: 测试**：既有 accumulator 测试改断言（model 用内置表有的 `deepseek-chat`）；新增"部分未定价 → None"、"无 usage 事件 → None"。
- [ ] **Step 4: 全套件 + clippy + Commit** `feat(metrics): per-usage historical costing`

### Task 4: 收尾

- [ ] `cargo test --workspace` 6 套件绿；`clippy -D warnings` 干净；release 体积无明显变化（纯逻辑）；e2e：临时 HOME + `~/.crab/pricing.toml` 给 mock 模型定价，跑 `--print` 后 `crab metrics` 显示非零 cost。

## Self-Review

类型一致：`cost_usd(p, model, ts, usage)`（Task 2 定义，Task 3 用）；`Version/Window`（Task 1/2 定义，Task 2 解析、Task 3 经 cost_usd 消费）；`TurnRow.cost_usd: Option<f64>` 不变，渲染不改。坑：① untagged serde 不用（Rates 全 default 会吞任何表），改手走 toml::Value；② 跨夜窗口匹配用 `rem_euclid` 防负 ts；③ 内置表 since 占位 epoch 保持旧行为，禁止臆造日期。
