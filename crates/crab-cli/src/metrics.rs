//! Metrics aggregation: a broadcast sink turns AgentEvents into per-turn rows
//! in a local SQLite database; `crab metrics` reports on them.
//!
//! 观测不改变系统: the sink is a best-effort independent broadcast receiver —
//! any failure here (db locked, disk full) only warns and never touches the run.

use crab_core::agent::AgentEvent;
use crab_core::obs::LlmUsage;
use std::path::Path;

/// USD per 1M tokens. Missing fields default to 0.0 (model partially priced).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A peak/valley window within one price version: when the call's local time
/// (per `tz_min`) falls in [start_min, end_min), the window's rates replace
/// the version's base rates. start > end means an overnight window.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start_min: i32,
    pub end_min: i32,
    pub tz_min: i32,
    pub rates: Rates,
}

impl Window {
    fn matches(&self, ts_ms: i64) -> bool {
        let local = (((ts_ms / 60000) + self.tz_min as i64).rem_euclid(1440)) as i32;
        if self.start_min < self.end_min {
            local >= self.start_min && local < self.end_min
        } else if self.start_min > self.end_min {
            local >= self.start_min || local < self.end_min
        } else {
            false // an empty window never matches
        }
    }
}

/// One entry in a model's price history: effective from `since` (epoch ms),
/// until the next version's `since`.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub since: i64,
    pub base: Rates,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pricing {
    /// Model name → versions sorted ascending by `since`.
    pub models: std::collections::HashMap<String, Vec<Version>>,
}

// ---- time primitives (no chrono dependency) ----

/// Howard Hinnant's days_from_civil. 1970-01-01 → 0.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC3339 subset `YYYY-MM-DDTHH:MM:SS(Z|±HH:MM)` → epoch millis. None if invalid.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || b.get(10) != Some(&b'T')
        || b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let y = num(0..4)?;
    let mo = num(5..7)?;
    let d = num(8..10)?;
    let h = num(11..13)?;
    let mi = num(14..16)?;
    let sec = num(17..19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let tz_min: i64 = match &s[19..] {
        "Z" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) && &tz[3..4] == ":" => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let th: i64 = tz[1..3].parse().ok()?;
            let tm: i64 = tz[4..6].parse().ok()?;
            if th > 23 || tm > 59 {
                return None;
            }
            sign * (th * 60 + tm)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3_600 + mi * 60 + sec - tz_min * 60) * 1000)
}

/// `+08:00` / `-05:30` / `Z` → offset minutes. None if invalid.
fn parse_tz(s: &str) -> Option<i32> {
    match s {
        "Z" => Some(0),
        _ if s.len() == 6 && (s.starts_with('+') || s.starts_with('-')) && &s[3..4] == ":" => {
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let h: i32 = s[1..3].parse().ok()?;
            let m: i32 = s[4..6].parse().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            Some(sign * (h * 60 + m))
        }
        _ => None,
    }
}

/// `"00:30"` → minutes since local midnight. None if invalid.
fn parse_hhmm(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return None;
    }
    let h: i32 = s[0..2].parse().ok()?;
    let m: i32 = s[3..5].parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

// ---- pricing parse / merge / resolve ----

/// TOML shapes we accept. We walk `toml::Value` manually (instead of untagged
/// serde) because a fully-defaulted `Rates` would match ANY table and break
/// shape discrimination.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct VersionToml {
    since: Option<String>,
    #[serde(flatten)]
    rates: Rates,
    windows: Vec<WindowToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct WindowToml {
    start: String,
    end: String,
    tz: String,
    #[serde(flatten)]
    rates: Rates,
}

fn version_from_toml(v: &toml::Value, warn: &str) -> Option<Version> {
    let vt: Result<VersionToml, _> = v.clone().try_into();
    let vt = match vt {
        Ok(vt) => vt,
        Err(e) => {
            eprintln!("[crab] pricing: {warn}: serde error: {e}");
            return None;
        }
    };
    let since = match &vt.since {
        Some(s) => match parse_rfc3339(s) {
            Some(ms) => ms,
            None => {
                eprintln!("[crab] pricing: {warn}: bad since {s:?}, version skipped");
                return None;
            }
        },
        None => 0, // flat legacy entry: always in effect
    };
    let windows = vt
        .windows
        .iter()
        .filter_map(|w| {
            let (Some(start_min), Some(end_min), Some(tz_min)) =
                (parse_hhmm(&w.start), parse_hhmm(&w.end), parse_tz(&w.tz))
            else {
                eprintln!("[crab] pricing: {warn}: bad window {:?}-{:?} tz {:?}, skipped", w.start, w.end, w.tz);
                return None;
            };
            Some(Window { start_min, end_min, tz_min, rates: w.rates })
        })
        .collect();
    Some(Version { since, base: vt.rates, windows })
}

pub fn parse_pricing(toml_str: &str) -> Pricing {
    let mut pricing = Pricing::default();
    // NOTE: `str::parse::<toml::Value>()` in toml 1.x parses a single VALUE,
    // not a document — use the serde entry point for documents (as config.rs does).
    let value = match toml::from_str::<toml::Value>(toml_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[crab] pricing: toml parse error: {e}");
            return pricing;
        }
    };
    let Some(models) = value.get("models").and_then(|m| m.as_table()) else {
        eprintln!("[crab] pricing: no [models] table in {:?}", &toml_str[..toml_str.len().min(60)]);
        return pricing;
    };
    for (name, model_value) in models {
        let versions: Vec<Version> = match model_value.get("versions").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| version_from_toml(v, &format!("model {name}")))
                .collect(),
            None => match version_from_toml(model_value, &format!("model {name}")) {
                Some(v) => vec![v],
                None => Vec::new(),
            },
        };
        if !versions.is_empty() {
            pricing.models.insert(name.clone(), versions);
        }
    }
    for versions in pricing.models.values_mut() {
        versions.sort_by_key(|v| v.since);
    }
    pricing
}

/// Pricing embedded at compile time (workspace root pricing.toml).
pub fn embedded_pricing() -> Pricing {
    parse_pricing(include_str!("../pricing.toml"))
}

/// Embedded defaults overlaid with the user's `~/.crab/pricing.toml`.
pub fn load_pricing() -> Pricing {
    let user_file = std::env::home_dir().map(|h| h.join(".crab").join("pricing.toml"));
    load_pricing_from(user_file.as_deref())
}

/// The testable seam: overlay the user file (if any) on the embedded table.
/// Version chains merge by `since`: same since → fields override when
/// non-zero, user windows replace embedded ones when non-empty.
fn load_pricing_from(user_file: Option<&Path>) -> Pricing {
    let mut pricing = embedded_pricing();
    let Some(path) = user_file else {
        return pricing;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return pricing;
    };
    let overlay = parse_pricing(&text);
    if overlay.models.is_empty() {
        eprintln!("[crab] pricing: {} parsed but has no usable [models.*] tables", path.display());
    }
    for (name, user_versions) in overlay.models {
        let entry = pricing.models.entry(name).or_default();
        for uv in user_versions {
            match entry.iter_mut().find(|e| e.since == uv.since) {
                Some(existing) => {
                    merge_rates(&mut existing.base, &uv.base);
                    if !uv.windows.is_empty() {
                        existing.windows = uv.windows;
                    }
                }
                None => entry.push(uv),
            }
        }
        entry.sort_by_key(|v| v.since);
    }
    pricing
}

fn merge_rates(dst: &mut Rates, src: &Rates) {
    if src.input != 0.0 { dst.input = src.input; }
    if src.output != 0.0 { dst.output = src.output; }
    if src.cache_read != 0.0 { dst.cache_read = src.cache_read; }
    if src.cache_write != 0.0 { dst.cache_write = src.cache_write; }
}

/// Cost in USD for one usage AT THE CALL TIME `ts_ms`: the last version with
/// `since <= ts_ms`, its base rates unless a peak/valley window matches.
/// None when the model is unknown or ts predates its first version.
pub fn cost_usd(p: &Pricing, model: &str, ts_ms: i64, u: &LlmUsage) -> Option<f64> {
    let versions = p.models.get(model)?;
    let v = versions.iter().rev().find(|v| v.since <= ts_ms)?;
    let rates = v
        .windows
        .iter()
        .find(|w| w.matches(ts_ms))
        .map(|w| &w.rates)
        .unwrap_or(&v.base);
    Some(
        (u.prompt as f64 * rates.input
            + u.completion as f64 * rates.output
            + u.cache_read as f64 * rates.cache_read
            + u.cache_write as f64 * rates.cache_write)
            / 1_000_000.0,
    )
}

/// One finished turn = one row.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRow {
    pub session_id: String,
    pub run_id: u64,
    pub ts: u64,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_read: u32,
    pub cache_write: u32,
    pub cost_usd: Option<f64>,
    pub llm_calls: u32,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub duration_ms: u64,
    pub interrupted: bool,
}

/// Consumes AgentEvents for one turn and produces a TurnRow at `Finished`.
pub struct RunAccumulator {
    pricing: Pricing,
    provider: String,
    model: String,
    started: Option<std::time::Instant>,
    ts: u64,
    usage: LlmUsage,
    llm_calls: u32,
    tool_calls: u32,
    tool_errors: u32,
    interrupted: bool,
    /// Sum of per-call costs. Sticky None once any call is unpriced.
    cost: Option<f64>,
    usage_events: u32,
}

impl RunAccumulator {
    pub fn new(pricing: Pricing, provider: String, model: String) -> Self {
        Self {
            pricing,
            provider,
            model,
            started: None,
            ts: 0,
            usage: LlmUsage::default(),
            llm_calls: 0,
            tool_calls: 0,
            tool_errors: 0,
            interrupted: false,
            cost: Some(0.0),
            usage_events: 0,
        }
    }

    /// Feed one event. Returns Some(TurnRow) when the turn finished.
    pub fn on_event(&mut self, ev: &AgentEvent) -> Option<TurnRow> {
        match ev {
            AgentEvent::TurnStarted { .. } => {
                self.started = Some(std::time::Instant::now());
                self.ts = crab_core::util::now_ms();
                self.usage = LlmUsage::default();
                self.llm_calls = 0;
                self.tool_calls = 0;
                self.tool_errors = 0;
                self.interrupted = false;
                self.cost = Some(0.0);
                self.usage_events = 0;
                None
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                // One Usage per llm call; count calls by usage frames when the
                // provider reports them, else the Finished path still works.
                self.usage.prompt += prompt_tokens;
                self.usage.completion += completion_tokens;
                self.usage.cache_read += cache_read_tokens;
                self.usage.cache_write += cache_write_tokens;
                self.llm_calls += 1;
                self.usage_events += 1;
                // Price each call at its own time: peak/valley windows and
                // version changes can flip mid-turn.
                let event_usage = LlmUsage {
                    prompt: *prompt_tokens,
                    completion: *completion_tokens,
                    cache_read: *cache_read_tokens,
                    cache_write: *cache_write_tokens,
                };
                match cost_usd(&self.pricing, &self.model, crab_core::util::now_ms() as i64, &event_usage) {
                    Some(c) => self.cost = self.cost.map(|sum| sum + c),
                    None => self.cost = None, // sticky: a turn is priced in full or not at all
                }
                None
            }
            AgentEvent::ToolCallFinished { is_error, .. } => {
                self.tool_calls += 1;
                if *is_error {
                    self.tool_errors += 1;
                }
                None
            }
            AgentEvent::Finished { reason } => {
                let started = self.started.take()?;
                self.interrupted = matches!(reason, crab_core::types::StopReason::Aborted);
                // No usage frames at all -> the provider told us nothing; cost
                // is unknown rather than zero.
                let cost = if self.usage_events == 0 { None } else { self.cost };
                Some(TurnRow {
                    session_id: String::new(), // filled by the sink
                    run_id: 0,                 // filled by the sink
                    ts: self.ts,
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    prompt_tokens: self.usage.prompt,
                    completion_tokens: self.usage.completion,
                    cache_read: self.usage.cache_read,
                    cache_write: self.usage.cache_write,
                    cost_usd: cost,
                    llm_calls: self.llm_calls,
                    tool_calls: self.tool_calls,
                    tool_errors: self.tool_errors,
                    duration_ms: started.elapsed().as_millis() as u64,
                    interrupted: self.interrupted,
                })
            }
            _ => None,
        }
    }
}

/// SQLite-backed metrics store. WAL mode; all writes are best-effort.
pub struct MetricsDb {
    conn: rusqlite::Connection,
}

impl MetricsDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS turns (
              session_id TEXT NOT NULL, run_id INTEGER NOT NULL,
              ts INTEGER NOT NULL,
              provider TEXT, model TEXT,
              prompt_tokens INTEGER, completion_tokens INTEGER,
              cache_read INTEGER, cache_write INTEGER,
              cost_usd REAL,
              llm_calls INTEGER, tool_calls INTEGER, tool_errors INTEGER,
              duration_ms INTEGER, interrupted INTEGER,
              PRIMARY KEY (session_id, run_id)
            );
            CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);",
        )?;
        Ok(Self { conn })
    }

    pub fn record_turn(&self, r: &TurnRow) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO turns
             (session_id, run_id, ts, provider, model, prompt_tokens, completion_tokens,
              cache_read, cache_write, cost_usd, llm_calls, tool_calls, tool_errors,
              duration_ms, interrupted)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                r.session_id,
                r.run_id as i64,
                r.ts as i64,
                r.provider,
                r.model,
                r.prompt_tokens as i64,
                r.completion_tokens as i64,
                r.cache_read as i64,
                r.cache_write as i64,
                r.cost_usd,
                r.llm_calls as i64,
                r.tool_calls as i64,
                r.tool_errors as i64,
                r.duration_ms as i64,
                r.interrupted as i64,
            ],
        )?;
        Ok(())
    }

    /// (day_bucket, duration_ms) pairs within the window, for percentile math in Rust.
    fn durations(&self, since_ms: i64) -> anyhow::Result<Vec<(i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts/86400000, duration_ms FROM turns WHERE ts >= ?1")?;
        let rows = stmt.query_map([since_ms], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Per-day aggregates over the last `days` days.
    pub fn daily_report(&self, days: u32) -> anyhow::Result<Vec<DailyRow>> {
        let since = crab_core::util::now_ms() as i64 - days as i64 * 86_400_000;
        let mut stmt = self.conn.prepare(
            "SELECT ts/86400000, COUNT(*), SUM(prompt_tokens), SUM(completion_tokens),
                    SUM(cache_read), SUM(cache_write), SUM(cost_usd), COUNT(cost_usd),
                    SUM(tool_calls), SUM(tool_errors)
             FROM turns WHERE ts >= ?1 GROUP BY ts/86400000 ORDER BY 1",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(DailyRow {
                day_ms: r.get::<_, i64>(0)? * 86_400_000,
                turns: r.get(1)?,
                prompt_tokens: r.get::<_, i64>(2).unwrap_or(0) as u64,
                completion_tokens: r.get::<_, i64>(3).unwrap_or(0) as u64,
                cache_read: r.get::<_, i64>(4).unwrap_or(0) as u64,
                cache_write: r.get::<_, i64>(5).unwrap_or(0) as u64,
                cost_usd: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                priced_turns: r.get::<_, i64>(7).unwrap_or(0),
                tool_calls: r.get::<_, i64>(8).unwrap_or(0) as u64,
                tool_errors: r.get::<_, i64>(9).unwrap_or(0) as u64,
                ..Default::default()
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        // Percentiles are computed per day bucket from the raw durations.
        let all = self.durations(since)?;
        for row in &mut out {
            let bucket = row.day_ms / 86_400_000;
            let mut ds: Vec<i64> = all
                .iter()
                .filter(|(b, _)| *b == bucket)
                .map(|(_, d)| *d)
                .collect();
            ds.sort_unstable();
            row.dur_p50_ms = percentile(&ds, 50);
            row.dur_p90_ms = percentile(&ds, 90);
        }
        Ok(out)
    }

    /// Per-model aggregates over the last `days` days.
    pub fn model_report(&self, days: u32) -> anyhow::Result<Vec<ModelRow>> {
        let since = crab_core::util::now_ms() as i64 - days as i64 * 86_400_000;
        let mut stmt = self.conn.prepare(
            "SELECT model, COUNT(*), SUM(prompt_tokens), SUM(completion_tokens),
                    SUM(cache_read), SUM(cache_write), SUM(cost_usd), COUNT(cost_usd)
             FROM turns WHERE ts >= ?1 GROUP BY model ORDER BY 7 DESC",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(ModelRow {
                model: r.get(0)?,
                turns: r.get(1)?,
                prompt_tokens: r.get::<_, i64>(2).unwrap_or(0) as u64,
                completion_tokens: r.get::<_, i64>(3).unwrap_or(0) as u64,
                cache_read: r.get::<_, i64>(4).unwrap_or(0) as u64,
                cache_write: r.get::<_, i64>(5).unwrap_or(0) as u64,
                cost_usd: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                priced_turns: r.get::<_, i64>(7).unwrap_or(0),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn percentile(sorted: &[i64], p: u64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as u64 - 1) * p / 100) as usize;
    sorted[idx]
}

#[derive(Debug, Default, serde::Serialize)]
pub struct DailyRow {
    pub day_ms: i64,
    pub turns: i64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub priced_turns: i64,
    #[serde(skip)]
    pub dur_p50_ms: i64,
    #[serde(skip)]
    pub dur_p90_ms: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct ModelRow {
    pub model: String,
    pub turns: i64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
    pub priced_turns: i64,
}

fn kfmt(n: u64) -> String {
    if n >= 1000 {
        format!("{}.{:01}k", n / 1000, (n % 1000) / 100)
    } else {
        n.to_string()
    }
}

/// `day_ms` bucket -> `YYYY-MM-DD` (Howard Hinnant's civil-from-days; no chrono dep).
fn day_label(day_ms: i64) -> String {
    let z = day_ms / 86_400_000 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", y + i64::from(m <= 2), m, d)
}

/// Render the text report.
pub fn render_report(daily: &[DailyRow], models: &[ModelRow], days: u32) -> String {
    let mut out = format!("crab metrics · last {days} day(s)\n\n");
    out.push_str("BY DAY\n");
    if daily.is_empty() {
        out.push_str("  (no turns recorded)\n");
    }
    for d in daily {
        let err_rate = if d.tool_calls > 0 {
            d.tool_errors as f64 / d.tool_calls as f64 * 100.0
        } else {
            0.0
        };
        let cost = if d.priced_turns == 0 {
            "      -".to_string()
        } else {
            format!("${:.4}", d.cost_usd)
        };
        out.push_str(&format!(
            "  {}  turns {:>3}  ↑{} ↓{}  cr{} cw{}  cost {:>8}  tool-err {:.0}%  p50 {}ms p90 {}ms\n",
            day_label(d.day_ms),
            d.turns,
            kfmt(d.completion_tokens),
            kfmt(d.prompt_tokens),
            kfmt(d.cache_read),
            kfmt(d.cache_write),
            cost,
            err_rate,
            d.dur_p50_ms,
            d.dur_p90_ms,
        ));
    }
    out.push_str("\nBY MODEL\n");
    if models.is_empty() {
        out.push_str("  (no turns recorded)\n");
    }
    for m in models {
        let cost = if m.priced_turns == 0 {
            "      -".to_string()
        } else {
            format!("${:.4}", m.cost_usd)
        };
        out.push_str(&format!(
            "  {:<28} turns {:>3}  ↑{} ↓{}  cost {:>8}\n",
            m.model,
            m.turns,
            kfmt(m.completion_tokens),
            kfmt(m.prompt_tokens),
            cost,
        ));
    }
    out
}

/// `crab metrics [days] [--json]`.
pub fn report(days: u32, json: bool) -> anyhow::Result<()> {
    let db_path = std::env::current_dir()?.join(".crab").join("metrics.sqlite");
    if !db_path.exists() {
        anyhow::bail!("no metrics database at {} — run a conversation first", db_path.display());
    }
    let db = MetricsDb::open(&db_path)?;
    let daily = db.daily_report(days)?;
    let models = db.model_report(days)?;
    if json {
        #[derive(serde::Serialize)]
        struct Report<'a> {
            days: u32,
            daily: &'a [DailyRow],
            models: &'a [ModelRow],
        }
        println!("{}", serde_json::to_string_pretty(&Report { days, daily: &daily, models: &models })?);
    } else {
        print!("{}", render_report(&daily, &models, days));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_core::types::StopReason;

    /// Versioned fixture: deepseek-chat with an old price and a newer price
    /// (effective 2025-09-06 16:30 +08:00) that carries a valley window
    /// 00:30-08:30 (+08:00).
    fn pricing_fixture() -> Pricing {
        parse_pricing(
            r#"
[models.deepseek-chat]
[[models.deepseek-chat.versions]]
since = "2025-09-06T16:30:00+08:00"
input = 0.99
output = 2.99
[[models.deepseek-chat.versions.windows]]
start = "00:30"
end = "08:30"
tz = "+08:00"
input = 0.50
output = 1.50
[[models.deepseek-chat.versions]]
since = "2024-12-01T00:00:00+08:00"
input = 0.27
output = 1.10
cache_read = 0.07
cache_write = 0.07
"#,
        )
    }

    #[test]
    fn rfc3339_parses_known_values() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2024-01-01T00:00:00Z"), Some(19_723 * 86_400_000));
        assert_eq!(
            parse_rfc3339("2024-01-01T00:00:00+08:00"),
            Some(19_723 * 86_400_000 - 8 * 3_600_000)
        );
        assert_eq!(
            parse_rfc3339("2024-01-01T00:00:00-05:30"),
            Some(19_723 * 86_400_000 + 5 * 3_600_000 + 30 * 60_000)
        );
        assert!(parse_rfc3339("not-a-date").is_none());
        assert!(parse_rfc3339("2024-01-01").is_none());
        assert!(parse_rfc3339("2024-13-01T00:00:00Z").is_none());
    }

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
        let p = pricing_fixture();
        let v = &p.models["deepseek-chat"];
        assert_eq!(v.len(), 2);
        assert!(v[0].since < v[1].since, "versions sort ascending by since");
        assert_eq!(v[1].windows.len(), 1);
        assert_eq!(v[1].windows[0].start_min, 30);
    }

    #[test]
    fn window_matching_includes_overnight() {
        let mk = |start_min: i32, end_min: i32| Window {
            start_min,
            end_min,
            tz_min: 8 * 60,
            rates: Rates::default(),
        };
        let valley = mk(30, 8 * 60 + 30);
        assert!(valley.matches(6 * 60 * 60_000 - 8 * 60 * 60_000)); // 06:00 +08:00
        assert!(!valley.matches(12 * 60 * 60_000 - 8 * 60 * 60_000)); // 12:00 +08:00
        let overnight = mk(16 * 60 + 30, 30);
        assert!(overnight.matches(20 * 60 * 60_000 - 8 * 60 * 60_000)); // 20:00 local
        assert!(overnight.matches(-8 * 3_600_000)); // 00:00 local (ts = UTC-8h)
        assert!(!overnight.matches(8 * 60 * 60_000 - 8 * 60 * 60_000)); // 08:00 +08:00
    }

    #[test]
    fn cost_resolves_by_timestamp_and_peak_valley() {
        let p = pricing_fixture();
        let u = LlmUsage { prompt: 1_000_000, ..Default::default() };
        let t_old = parse_rfc3339("2025-01-01T12:00:00+08:00").unwrap();
        let t_peak = parse_rfc3339("2025-10-01T12:00:00+08:00").unwrap();
        let t_valley = parse_rfc3339("2025-10-01T04:00:00+08:00").unwrap();
        assert!((cost_usd(&p, "deepseek-chat", t_old, &u).unwrap() - 0.27).abs() < 1e-9);
        assert!((cost_usd(&p, "deepseek-chat", t_peak, &u).unwrap() - 0.99).abs() < 1e-9);
        assert!((cost_usd(&p, "deepseek-chat", t_valley, &u).unwrap() - 0.50).abs() < 1e-9);
        // before the first version / unknown model -> None
        assert!(cost_usd(&p, "deepseek-chat", 0, &u).is_none());
        assert!(cost_usd(&p, "gpt-99", t_peak, &u).is_none());
    }

    #[test]
    fn accumulator_counts_usage_tools_and_finish() {
        let mut acc = RunAccumulator::new(
            pricing_fixture(),
            "deepseek".into(),
            "deepseek-chat".into(),
        );
        assert!(acc.on_event(&AgentEvent::TurnStarted { turn: 0 }).is_none());
        for _ in 0..2 {
            assert!(acc
                .on_event(&AgentEvent::Usage {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    cache_read_tokens: 5,
                    cache_write_tokens: 1,
                })
                .is_none());
        }
        assert!(acc
            .on_event(&AgentEvent::ToolCallFinished {
                id: "t".into(),
                result: "ok".into(),
                is_error: false,
            })
            .is_none());
        assert!(acc
            .on_event(&AgentEvent::ToolCallFinished {
                id: "t2".into(),
                result: "nope".into(),
                is_error: true,
            })
            .is_none());
        let row = acc
            .on_event(&AgentEvent::Finished { reason: StopReason::EndTurn })
            .expect("Finished produces the row");
        assert_eq!(row.llm_calls, 2);
        assert_eq!(row.tool_calls, 2);
        assert_eq!(row.tool_errors, 1);
        assert_eq!(row.prompt_tokens, 200);
        assert!(row.cost_usd.unwrap() > 0.0);
        assert!(!row.interrupted);
    }

    #[test]
    fn user_pricing_file_overlays_embedded_table() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("pricing.toml");
        std::fs::write(
            &user,
            r#"
[models.deepseek-chat]
input = 0.99

[models.deepseek-flash]
input = 0.1
output = 0.2
"#,
        )
        .unwrap();
        let p = load_pricing_from(Some(&user));
        // flat user entry (since=0) merges with the embedded since=0 version:
        // user's non-zero field wins, embedded fields survive.
        let v0 = p.models["deepseek-chat"].iter().find(|v| v.since == 0).unwrap();
        assert_eq!(v0.base.input, 0.99);
        assert_eq!(v0.base.output, 1.10);
        // user-only model appears
        assert!(p.models.contains_key("deepseek-flash"));
        // missing user file: embedded only
        assert_eq!(load_pricing_from(None), embedded_pricing());
    }

    #[test]
    fn overlay_new_version_extends_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("pricing.toml");
        std::fs::write(
            &user,
            r#"
[models.deepseek-chat]
[[models.deepseek-chat.versions]]
since = "2026-01-01T00:00:00+08:00"
input = 1.50
output = 4.00
"#,
        )
        .unwrap();
        let p = load_pricing_from(Some(&user));
        let chain = &p.models["deepseek-chat"];
        assert_eq!(chain.len(), 2, "embedded epoch version + user future version");
        assert!(chain[0].since < chain[1].since);
        assert_eq!(chain[1].base.input, 1.50);
    }

    #[test]
    fn unpriced_models_render_dash_not_zero() {
        let daily = vec![DailyRow {
            day_ms: 19_723 * 86_400_000,
            turns: 2,
            priced_turns: 0,
            ..Default::default()
        }];
        let models = vec![ModelRow {
            model: "deepseek-flash".into(),
            turns: 2,
            priced_turns: 0,
            prompt_tokens: 10_000,
            completion_tokens: 1_000,
            cache_read: 0,
            cache_write: 0,
            cost_usd: 0.0,
        }];
        let out = render_report(&daily, &models, 7);
        assert!(out.contains("cost        -"), "{out}");
        assert!(!out.contains("$0.0000"), "{out}");
    }

    #[test]
    fn day_label_formats_known_dates() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(19_723 * 86_400_000), "2024-01-01");
        assert_eq!(day_label(20_745 * 86_400_000), "2026-10-19");
    }

    #[test]
    fn turn_without_usage_events_has_unknown_cost() {
        let mut acc = RunAccumulator::new(
            pricing_fixture(),
            "deepseek".into(),
            "deepseek-chat".into(),
        );
        acc.on_event(&AgentEvent::TurnStarted { turn: 0 });
        let row = acc
            .on_event(&AgentEvent::Finished { reason: StopReason::EndTurn })
            .unwrap();
        assert!(row.cost_usd.is_none(), "no usage frames -> unknown, not zero");
    }

    #[test]
    fn db_records_and_reports() {
        let db = MetricsDb::open(Path::new(":memory:")).unwrap();
        let now = crab_core::util::now_ms() as i64;
        db.record_turn(&TurnRow {
            session_id: "s1".into(),
            run_id: 0,
            ts: now as u64,
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            prompt_tokens: 1000,
            completion_tokens: 100,
            cache_read: 0,
            cache_write: 0,
            cost_usd: Some(0.001),
            llm_calls: 1,
            tool_calls: 1,
            tool_errors: 0,
            duration_ms: 500,
            interrupted: false,
        })
        .unwrap();
        let daily = db.daily_report(7).unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].turns, 1);
        assert_eq!(daily[0].dur_p50_ms, 500);
        let models = db.model_report(7).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "deepseek-chat");
        let out = render_report(&daily, &models, 7);
        assert!(out.contains("BY DAY"), "{out}");
        assert!(out.contains("BY MODEL"), "{out}");
    }
}
