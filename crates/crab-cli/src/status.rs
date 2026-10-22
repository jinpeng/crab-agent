//! Bottom status line for the interactive UI.
//!
//! A [`StatusLine`] holds the few facts worth showing at a glance — model,
//! agent state, token usage, context budget and working directory — and
//! renders them as one plain (unstyled) line that is then clipped to the
//! terminal width, so the caller can print it on the last row.

use std::path::PathBuf;

use crate::width::{truncate_to_width, visible_width};

/// What the agent is doing right now.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RunState {
    /// Waiting for user input.
    Idle,
    /// Streaming an assistant response.
    Streaming,
    /// Running a tool call.
    WaitingTool,
    /// Blocked on a user confirmation prompt.
    WaitingConfirm,
}

impl RunState {
    /// The lowercase word used in the status line.
    fn word(self) -> &'static str {
        match self {
            RunState::Idle => "idle",
            RunState::Streaming => "streaming",
            RunState::WaitingTool => "tool",
            RunState::WaitingConfirm => "confirm",
        }
    }
}

/// A single-line summary of the session state.
pub struct StatusLine {
    provider_model: String,
    state: RunState,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_write: u64,
    /// Cumulative session cost (USD) for the `$` segment; None = unpriced.
    cost: Option<f64>,
    context_used: Option<u64>,
    context_window: Option<u64>,
    cwd: PathBuf,
    branch: Option<String>,
}

impl StatusLine {
    /// Build a status line for a fresh session.
    ///
    /// Starts [`RunState::Idle`] with no token or context usage recorded;
    /// use the `set_*` methods to update it as the session progresses.
    pub fn new(provider_model: String, cwd: PathBuf) -> Self {
        Self {
            provider_model,
            state: RunState::Idle,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cache_write: 0,
            cost: None,
            context_used: None,
            context_window: None,
            cwd,
            branch: None,
        }
    }

    /// Replace the current agent state.
    pub fn set_state(&mut self, state: RunState) {
        self.state = state;
    }

    /// Update the provider/model label (e.g. after `/model`).
    pub fn set_provider_model(&mut self, provider_model: String) {
        self.provider_model = provider_model;
    }

    /// Accumulate a response's four-way usage and refresh the context estimate.
    /// Cache read/write follow pi's status-line convention: `R`/`W` segments.
    pub fn add_usage(&mut self, u: crab_core::obs::LlmUsage) {
        self.tokens_in += u.prompt as u64;
        self.tokens_out += u.completion as u64;
        self.cache_read += u.cache_read as u64;
        self.cache_write += u.cache_write as u64;
        // The last request's prompt size is the closest proxy for how much of
        // the context window is occupied right now.
        self.context_used = Some(u.prompt as u64);
    }

    /// Seed cumulative counters from a replayed session (`--continue`) without
    /// touching the context estimate — occupancy is seeded separately via
    /// `set_context`, and a seed must not masquerade as the latest prompt.
    pub fn seed_usage(&mut self, u: crab_core::obs::LlmUsage) {
        self.tokens_in += u.prompt as u64;
        self.tokens_out += u.completion as u64;
        self.cache_read += u.cache_read as u64;
        self.cache_write += u.cache_write as u64;
    }

    /// Replace the cumulative cost (`--continue` seed or a live pricer fold).
    /// None hides the segment entirely (pi shows cost only when non-zero).
    pub fn set_cost(&mut self, cost: Option<f64>) {
        self.cost = cost;
    }

    /// Replace the cached git branch (refreshed by the TUI's async task).
    pub fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    /// Replace the context budget. Pass `None` for either value when it is
    /// unknown; without a window the segment shows the absolute count only.
    pub fn set_context(&mut self, used: Option<u64>, window: Option<u64>) {
        self.context_used = used;
        self.context_window = window;
    }

    /// Row 1 (below the input box): ` ~/path (branch) `.
    pub fn format_cwd_row(&self, width: usize) -> String {
        let mut label = self.cwd_label();
        if let Some(b) = &self.branch {
            label.push_str(&format!(" ({b})"));
        }
        truncate_to_width(&format!(" {label} "), width)
    }

    /// Row 2: left `↑in ↓out [R<cacheRead> [W<cacheWrite>]] [CH<hit%>] [$cost]
    /// <ctx%>/<window> <state>` (pi's footer: ↑=input ↓=output, R/W cumulative
    /// cache tokens, CH = cache HIT RATE one decimal, $ = cumulative cost,
    /// context as one-decimal percent of the window) and right
    /// `(provider) model`, right-aligned to `width`. When too narrow the model
    /// label is dropped and only the left part is kept (clipped).
    pub fn format_status_row(&self, width: usize) -> String {
        let mut left = format!("↑{} ↓{}", kfmt(self.tokens_in), kfmt(self.tokens_out));
        if self.cache_read > 0 {
            left.push_str(&format!(" R{}", kfmt(self.cache_read)));
        }
        if self.cache_write > 0 {
            left.push_str(&format!(" W{}", kfmt(self.cache_write)));
        }
        if let Some(ch) = self.cache_hit_segment() {
            left.push(' ');
            left.push_str(&ch);
        }
        if let Some(cost) = self.cost.filter(|c| *c > 0.0) {
            left.push_str(&format!(" ${cost:.3}"));
        }
        if let Some(ctx) = self.context_segment() {
            left.push(' ');
            left.push_str(&ctx);
        }
        left.push(' ');
        left.push_str(self.state.word());

        let right = &self.provider_model;
        let total = visible_width(&left) + visible_width(right);
        if total + 3 <= width {
            // 2 edge spaces + at least 1 space between the segments.
            let middle = width - total - 2;
            return format!(" {left}{}{right} ", " ".repeat(middle));
        }
        truncate_to_width(&format!(" {left} "), width)
    }

    /// `CH<pct>%` — the cache HIT RATE (pi's CH), cumulative:
    /// cache_read / prompt (prompt includes cache hits per the DeepSeek
    /// convention). Shown only when cache activity exists; one decimal.
    fn cache_hit_segment(&self) -> Option<String> {
        if self.cache_read == 0 && self.cache_write == 0 {
            return None;
        }
        let pct = 100.0 * self.cache_read as f64 / self.tokens_in.max(1) as f64;
        Some(format!("CH{pct:.1}%"))
    }

    /// `<pct>%/<window>` with one decimal (pi's `20.5%/1.0M`), or None when
    /// usage or the window is unknown.
    fn context_segment(&self) -> Option<String> {
        let used = self.context_used?;
        let window = self.context_window.filter(|w| *w > 0)?;
        let pct = used as f64 * 100.0 / window as f64;
        Some(format!("{pct:.1}%/{}", kfmt(window)))
    }

    /// The working directory, with a leading `$HOME` shortened to `~`.
    fn cwd_label(&self) -> String {
        let cwd = self.cwd.to_string_lossy();
        if let Some(home) = std::env::var_os("HOME") {
            let home = home.to_string_lossy();
            if let Some(rest) = cwd.strip_prefix(home.as_ref()) {
                if rest.is_empty() {
                    return "~".to_string();
                }
                if let Some(rest) = rest.strip_prefix('/') {
                    return format!("~/{rest}");
                }
            }
        }
        cwd.into_owned()
    }
}

/// Format a token count: `500` -> `"500"`, `1200` -> `"1.2k"`,
/// `128000` -> `"128.0k"`, `1048576` -> `"1.0M"`.
fn kfmt(n: u64) -> String {
    if n >= 1_000_000 {
        let whole = n / 1_000_000;
        let tenth = (n % 1_000_000) / 100_000;
        format!("{whole}.{tenth}M")
    } else if n >= 1000 {
        let whole = n / 1000;
        let tenth = (n % 1000) / 100;
        format!("{whole}.{tenth}k")
    } else {
        n.to_string()
    }
}

/// `git branch --show-current` in `cwd`; None outside a git work tree, on a
/// detached HEAD, or when git is unavailable.
pub fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::visible_width;

    fn home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/user"))
    }

    fn full() -> StatusLine {
        StatusLine {
            provider_model: "deepseek/deepseek-chat".to_string(),
            state: RunState::Streaming,
            tokens_in: 45_600,
            tokens_out: 12_300,
            cache_read: 910,
            cache_write: 0,
            cost: None,
            context_used: Some(8500),
            context_window: Some(128_000),
            cwd: home().join("projects/crab"),
            branch: Some("main".to_string()),
        }
    }

    #[test]
    fn kfmt_switches_to_k_at_a_thousand() {
        assert_eq!(kfmt(0), "0");
        assert_eq!(kfmt(500), "500");
        assert_eq!(kfmt(1200), "1.2k");
        assert_eq!(kfmt(128000), "128.0k");
        assert_eq!(kfmt(1048576), "1.0M");
        assert_eq!(kfmt(2_000_000), "2.0M");
    }

    #[test]
    fn add_usage_accumulates_four_way_and_context_takes_latest_prompt() {
        let mut s = StatusLine::new("m".into(), PathBuf::from("/"));
        s.add_usage(crab_core::obs::LlmUsage {
            prompt: 100,
            completion: 20,
            cache_read: 30,
            cache_write: 5,
        });
        s.add_usage(crab_core::obs::LlmUsage {
            prompt: 300,
            completion: 40,
            cache_read: 70,
            cache_write: 5,
        });
        assert_eq!(s.tokens_in, 400);
        assert_eq!(s.tokens_out, 60);
        assert_eq!(s.cache_read, 100);
        assert_eq!(s.cache_write, 10);
        assert_eq!(
            s.context_used,
            Some(300),
            "context tracks the latest prompt size"
        );
    }

    #[test]
    fn seed_usage_adds_counters_but_keeps_context_untouched() {
        let mut s = StatusLine::new("m".into(), PathBuf::from("/"));
        s.seed_usage(crab_core::obs::LlmUsage {
            prompt: 50_000,
            completion: 5_000,
            cache_read: 40_000,
            cache_write: 100,
        });
        assert_eq!(s.tokens_in, 50_000);
        assert_eq!(s.tokens_out, 5_000);
        assert_eq!(s.cache_read, 40_000);
        assert_eq!(s.cache_write, 100);
        assert_eq!(
            s.context_used, None,
            "a seed must not fake the latest prompt"
        );
        // 后续 add_usage 照常刷新 context。
        s.add_usage(crab_core::obs::LlmUsage {
            prompt: 100,
            ..Default::default()
        });
        assert_eq!(s.context_used, Some(100));
    }

    #[test]
    fn cwd_row_shows_branch_in_parens() {
        let row = full().format_cwd_row(200);
        assert!(row.contains("~/projects/crab (main)"), "got {row:?}");
        assert!(row.starts_with(' ') && row.ends_with(' '));
    }

    #[test]
    fn cwd_row_without_branch_has_no_parens() {
        let mut s = full();
        s.branch = None;
        let row = s.format_cwd_row(200);
        assert!(row.contains("~/projects/crab"), "got {row:?}");
        assert!(!row.contains('('), "got {row:?}");
    }

    #[test]
    fn status_row_is_exactly_width_with_model_right_aligned() {
        let row = full().format_status_row(80);
        assert_eq!(visible_width(&row), 80, "got {row:?}");
        assert!(row.contains("↑45.6k ↓12.3k"), "got {row:?}");
        // context: one-decimal percent + window (pi's 20.5%/1.0M)
        assert!(row.contains("6.6%/128.0k"), "got {row:?}"); // 8500/128000
        assert!(row.contains("streaming"), "got {row:?}");
        assert!(
            row.trim_end().ends_with("deepseek/deepseek-chat"),
            "got {row:?}"
        );
    }

    #[test]
    fn status_row_uses_pi_arrow_convention_with_cache_segments() {
        let row = full().format_status_row(80);
        // pi: ↑=input ↓=output；R/W=累计缓存 tokens（非零才显示）；
        // CH=缓存命中率（910/45600 ≈ 2.0%，一位小数）。
        assert!(row.contains("↑45.6k ↓12.3k"), "got {row:?}");
        assert!(row.contains("R910"), "got {row:?}");
        assert!(
            !row.contains('W'),
            "cache_write is 0 in the fixture: {row:?}"
        );
        assert!(row.contains("CH2.0%"), "got {row:?}");
    }

    #[test]
    fn status_row_shows_w_when_cache_write_nonzero() {
        let mut s = full();
        s.cache_write = 2_500;
        let row = s.format_status_row(80);
        assert!(row.contains("R910 W2.5k"), "got {row:?}");
    }

    #[test]
    fn status_row_shows_cost_when_priced() {
        let mut s = full();
        s.set_cost(Some(0.042_169));
        let row = s.format_status_row(80);
        assert!(row.contains("$0.042"), "got {row:?}");
        // 成本段在 context 段之前（pi 顺序）。
        assert!(
            row.find("$0.042").unwrap() < row.find("6.6%").unwrap(),
            "cost before context: {row:?}"
        );
        let mut free = full();
        free.set_cost(None);
        assert!(
            !free.format_status_row(80).contains('$'),
            "unpriced shows no cost"
        );
        let mut zero = full();
        zero.set_cost(Some(0.0));
        assert!(
            !zero.format_status_row(80).contains('$'),
            "zero cost shows no segment"
        );
    }

    #[test]
    fn status_row_drops_model_when_narrow() {
        let row = full().format_status_row(24);
        assert!(visible_width(&row) <= 24);
        assert!(row.contains("↑45.6k"), "numbers survive longest: {row:?}");
        assert!(!row.contains("deepseek"), "got {row:?}");
    }

    #[test]
    fn status_row_omits_context_without_window() {
        let mut s = full();
        s.context_window = None;
        let row = s.format_status_row(80);
        assert!(
            !row.contains("%/"),
            "no context segment without a window: {row:?}"
        );
    }

    #[test]
    fn git_branch_in_repo_and_outside() {
        if std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("git unavailable: skipping");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(
            git_branch(repo.path()).is_some(),
            "a fresh repo has an unborn branch"
        );

        let plain = tempfile::tempdir().unwrap();
        assert!(git_branch(plain.path()).is_none());
    }
}
