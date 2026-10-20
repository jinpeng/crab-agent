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

#[derive(Debug, Clone, Default)]
pub struct Pricing {
    pub models: std::collections::HashMap<String, Rates>,
}

pub fn parse_pricing(toml_str: &str) -> Pricing {
    #[derive(serde::Deserialize)]
    struct File {
        models: std::collections::HashMap<String, Rates>,
    }
    let parsed: Option<File> = toml::from_str(toml_str).ok();
    Pricing {
        models: parsed.map(|f| f.models).unwrap_or_default(),
    }
}

/// Pricing embedded at compile time (workspace root pricing.toml).
pub fn embedded_pricing() -> Pricing {
    parse_pricing(include_str!("../pricing.toml"))
}

/// Cost in USD for one usage, or None when the model has no rates.
pub fn cost_usd(p: &Pricing, model: &str, u: &LlmUsage) -> Option<f64> {
    let r = p.models.get(model)?;
    Some(
        (u.prompt as f64 * r.input
            + u.completion as f64 * r.output
            + u.cache_read as f64 * r.cache_read
            + u.cache_write as f64 * r.cache_write)
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
                let cost = cost_usd(&self.pricing, &self.model, &self.usage);
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
                    SUM(cache_read), SUM(cache_write), SUM(cost_usd),
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
                tool_calls: r.get::<_, i64>(7).unwrap_or(0) as u64,
                tool_errors: r.get::<_, i64>(8).unwrap_or(0) as u64,
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
                    SUM(cache_read), SUM(cache_write), SUM(cost_usd)
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
        out.push_str(&format!(
            "  {}  turns {:>3}  ↑{} ↓{}  cr{} cw{}  ${:.4}  tool-err {:.0}%  p50 {}ms p90 {}ms\n",
            day_label(d.day_ms),
            d.turns,
            kfmt(d.completion_tokens),
            kfmt(d.prompt_tokens),
            kfmt(d.cache_read),
            kfmt(d.cache_write),
            d.cost_usd,
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
        out.push_str(&format!(
            "  {:<28} turns {:>3}  ↑{} ↓{}  ${:.4}\n",
            m.model,
            m.turns,
            kfmt(m.completion_tokens),
            kfmt(m.prompt_tokens),
            m.cost_usd,
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

    fn pricing_fixture() -> Pricing {
        parse_pricing(
            r#"
[models.deepseek-chat]
input = 0.27
output = 1.10
cache_read = 0.07
cache_write = 0.07

[models.unknown-later]
input = 1.0
"#,
        )
    }

    #[test]
    fn pricing_parse_and_cost_with_cache() {
        let p = pricing_fixture();
        let u = LlmUsage { prompt: 1_000_000, completion: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000 };
        let cost = cost_usd(&p, "deepseek-chat", &u).unwrap();
        assert!((cost - (0.27 + 1.10 + 0.07 + 0.07)).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn unknown_or_partial_model_costs_none() {
        let p = pricing_fixture();
        let u = LlmUsage { prompt: 100, ..Default::default() };
        assert!(cost_usd(&p, "gpt-99", &u).is_none());
        // rates default to 0.0 for missing fields: price is computable but cheap
        assert!(cost_usd(&p, "unknown-later", &u).unwrap() < 0.001);
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
    fn day_label_formats_known_dates() {
        assert_eq!(day_label(0), "1970-01-01");
        assert_eq!(day_label(19_723 * 86_400_000), "2024-01-01");
        assert_eq!(day_label(20_745 * 86_400_000), "2026-10-19");
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
