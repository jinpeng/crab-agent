//! Bottom status line for the interactive UI.
//!
//! A [`StatusLine`] holds the few facts worth showing at a glance — model,
//! agent state, token usage, context budget and working directory — and
//! renders them as one plain (unstyled) line that is then clipped to the
//! terminal width, so the caller can print it on the last row.

use std::path::PathBuf;

use crate::width::truncate_to_width;

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
    tokens_used: u64,
    context_used: Option<u64>,
    context_window: Option<u64>,
    cwd: PathBuf,
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
            tokens_used: 0,
            context_used: None,
            context_window: None,
            cwd,
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

    /// Replace the number of tokens used so far.
    pub fn set_tokens_used(&mut self, tokens_used: u64) {
        self.tokens_used = tokens_used;
    }

    /// Replace the context budget. Pass `None` for either value when it is
    /// unknown; without a window the segment shows the absolute count only.
    pub fn set_context(&mut self, used: Option<u64>, window: Option<u64>) {
        self.context_used = used;
        self.context_window = window;
    }

    /// The configured context window, if any.
    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// Render the line, clipped to `width` visible columns.
    ///
    /// Shape (leading and trailing spaces included):
    /// ` <provider_model> · <state> · <tokens> · ctx <used>/<window> (<pct>%) · <cwd> `.
    /// The `ctx` segment is dropped entirely when no context usage is known.
    pub fn format(&self, width: usize) -> String {
        let mut parts = vec![
            self.provider_model.clone(),
            self.state.word().to_string(),
            kfmt(self.tokens_used),
        ];
        if let Some(ctx) = self.ctx_segment() {
            parts.push(ctx);
        }
        parts.push(self.cwd_label());
        let joined = parts.join(" · ");
        truncate_to_width(&format!(" {joined} "), width)
    }

    /// The context segment, or `None` when nothing is known about it.
    fn ctx_segment(&self) -> Option<String> {
        let used = self.context_used?;
        match self.context_window.filter(|w| *w > 0) {
            Some(window) => {
                // Round to nearest whole percent.
                let pct = (used.saturating_mul(100) + window / 2) / window;
                Some(format!("ctx {}/{} ({pct}%)", kfmt(used), kfmt(window)))
            }
            None => Some(format!("ctx {}", kfmt(used))),
        }
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
/// `128000` -> `"128.0k"`.
fn kfmt(n: u64) -> String {
    if n >= 1000 {
        let whole = n / 1000;
        let tenth = (n % 1000) / 100;
        format!("{whole}.{tenth}k")
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::visible_width;

    fn full() -> StatusLine {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/user"));
        StatusLine {
            provider_model: "anthropic/claude-sonnet-4".to_string(),
            state: RunState::Streaming,
            tokens_used: 1200,
            context_used: Some(8500),
            context_window: Some(128000),
            cwd: home.join("projects/crab"),
        }
    }

    #[test]
    fn kfmt_switches_to_k_at_a_thousand() {
        assert_eq!(kfmt(0), "0");
        assert_eq!(kfmt(500), "500");
        assert_eq!(kfmt(1200), "1.2k");
        assert_eq!(kfmt(128000), "128.0k");
    }

    #[test]
    fn full_status_line_contains_every_segment() {
        let line = full().format(200);
        for needle in [
            "anthropic/claude-sonnet-4",
            "streaming",
            "1.2k",
            "8.5k/128.0k",
            "7%",
            "projects/crab",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line:?}");
        }
        assert!(line.starts_with(' '), "no leading space: {line:?}");
        assert!(line.ends_with(' '), "no trailing space: {line:?}");
    }

    #[test]
    fn context_without_window_shows_absolute_count_and_no_percent() {
        let mut status = full();
        status.context_used = Some(500);
        status.context_window = None;
        let line = status.format(200);
        assert!(line.contains("ctx 500"), "{line:?}");
        assert!(!line.contains('%'), "unexpected percent: {line:?}");
    }

    #[test]
    fn no_context_omits_the_segment_entirely() {
        let mut status = full();
        status.context_used = None;
        status.context_window = None;
        let line = status.format(200);
        assert!(!line.contains("ctx"), "{line:?}");
        assert!(!line.contains("·  ·"), "gap left behind: {line:?}");
    }

    #[test]
    fn new_defaults_and_setters_are_reflected_in_format() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/user"));
        let mut status = StatusLine::new("openai/gpt-5".to_string(), home.join("work/crab"));

        let line = status.format(200);
        assert!(line.contains("openai/gpt-5"), "{line:?}");
        assert!(line.contains("idle"), "{line:?}");
        assert!(line.contains("· 0 ·"), "{line:?}");
        assert!(!line.contains("ctx"), "{line:?}");

        status.set_state(RunState::WaitingTool);
        status.set_tokens_used(1200);
        status.set_context(Some(8500), Some(128000));
        let line = status.format(200);
        for needle in [
            "openai/gpt-5",
            "tool",
            "1.2k",
            "ctx 8.5k/128.0k",
            "7%",
            "work/crab",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line:?}");
        }
        assert!(!line.contains("idle"), "stale state in {line:?}");

        status.set_state(RunState::Idle);
        status.set_tokens_used(0);
        status.set_context(None, None);
        let line = status.format(200);
        assert!(line.contains("idle"), "{line:?}");
        assert!(line.contains("· 0 ·"), "{line:?}");
        assert!(!line.contains("ctx"), "{line:?}");
    }

    #[test]
    fn truncates_to_a_narrow_width() {
        let line = full().format(40);
        assert!(visible_width(&line) <= 40, "too wide: {line:?}");
    }
}
