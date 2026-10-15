//! Plain streaming renderer for agent events.
//!
//! Deliberately *not* a full-screen TUI: no alternate screen, no ratatui, no
//! in-place cursor tricks. Text deltas are written as they arrive (so
//! `crab --print` pipes cleanly and stays tail-able), tool calls are rendered as
//! a small structured block colored through [`Theme`], and every rendered line
//! is clipped to the terminal width with [`crate::width`].

use crab_core::agent::AgentEvent;
use crossterm::terminal;
use std::io::Write;
use tokio::sync::broadcast::error::RecvError;

use crate::theme::Theme;
use crate::width::{truncate_to_width, visible_width};

/// Width assumed when the real terminal width cannot be queried (pipe, dumb
/// terminal, no tty).
const FALLBACK_WIDTH: usize = 80;

/// Leading marker of a tool call header line.
const TOOL_MARKER: &str = "●";
/// Marker of a tool call that succeeded.
const OK_MARKER: &str = "✓";
/// Marker of a tool call that failed.
const ERR_MARKER: &str = "✗";

/// Stream agent events to stdout until the *channel closes*, i.e. until every
/// sender is gone.
///
/// `--print` must not stop at `Finished`. The run loop (which publishes
/// `Finished`) and the provider→agent bridge task (which publishes `TextDelta`)
/// write to the same broadcast channel from different tasks, so a delta can be
/// enqueued *after* `Finished` — including after this task has already read it.
/// Only the channel closing proves that no further event can arrive, which is
/// why the caller drops the agent (all senders) before awaiting this task. The
/// closing newline is written last, so a straggler delta can never land after it.
pub async fn render_events_until_closed(mut rx: tokio::sync::broadcast::Receiver<AgentEvent>) {
    let theme = Theme::dark();
    let mut out = std::io::stdout();
    let mut finished = false;
    loop {
        match rx.recv().await {
            // Don't break: drain the stragglers the bridge task may still add.
            Ok(AgentEvent::Finished { .. }) => finished = true,
            Ok(ev) => render_event(&mut out, &theme, &ev),
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
    if finished {
        let _ = writeln!(out);
    }
    let _ = out.flush();
}

/// Write one non-terminal event to `out` and flush, so a piped stdout sees each
/// delta as it arrives. `Finished` is handled by the caller's loop.
fn render_event(out: &mut impl Write, theme: &Theme, ev: &AgentEvent) {
    match ev {
        AgentEvent::TextDelta(s) => {
            let _ = out.write_all(s.as_bytes());
        }
        AgentEvent::ToolCallStarted { name, .. } => {
            // The event carries no args yet, so only the header line shows.
            let _ = write!(out, "\n{}", format_tool_call(theme, name, "", None, false));
        }
        AgentEvent::ToolCallFinished {
            result, is_error, ..
        } => {
            let _ = write!(
                out,
                "{}",
                tool_result(theme, result, *is_error, terminal_width())
            );
        }
        // `Finished` (handled by the caller) and events with no rendering yet.
        _ => return,
    }
    let _ = out.flush();
}

/// Render a structured tool call block: the cyan `● <name> <args>` header line
/// and, when the call has already finished, the `✓`/`✗` result line.
///
/// Every line is newline-terminated, clipped to the terminal width, and carries
/// ANSI color, so the result can be written straight to stdout.
pub fn format_tool_call(
    t: &Theme,
    name: &str,
    args: &str,
    result: Option<&str>,
    is_error: bool,
) -> String {
    let width = terminal_width();
    let mut block = tool_header(t, name, args, width);
    if let Some(result) = result {
        block.push_str(&tool_result(t, result, is_error, width));
    }
    block
}

/// The cyan `● <name> <args>` header line, newline-terminated.
///
/// The name is always shown in full; `args` is dimmed and truncated so the line
/// never exceeds `width` visible columns.
fn tool_header(t: &Theme, name: &str, args: &str, width: usize) -> String {
    let head = t.tool_name(&format!("{TOOL_MARKER} {name}"));
    let mut line = match args.trim() {
        "" => head,
        args => {
            let avail = width.saturating_sub(visible_width(&head) + 1);
            format!("{head} {}", t.dim(&truncate_to_width(args, avail)))
        }
    };
    line.push('\n');
    line
}

/// The `✓`/`✗` result line: green/red marker plus the first line of `result`,
/// dimmed and truncated so the whole line fits in `width` visible columns.
fn tool_result(t: &Theme, result: &str, is_error: bool, width: usize) -> String {
    let marker = if is_error {
        t.error(ERR_MARKER)
    } else {
        t.success(OK_MARKER)
    };
    // Only the first line belongs on the one-line result summary.
    let first = result.lines().next().unwrap_or("").trim_end();
    if first.is_empty() {
        return format!("{marker}\n");
    }
    let avail = width.saturating_sub(visible_width(&marker) + 1);
    let body = t.dim(&truncate_to_width(first, avail));
    format!("{marker} {body}\n")
}

/// Current terminal width in columns, falling back to [`FALLBACK_WIDTH`].
fn terminal_width() -> usize {
    match terminal::size() {
        Ok((w, _)) if w > 0 => w as usize,
        _ => FALLBACK_WIDTH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_block_marks_a_check_and_keeps_name_and_args() {
        let t = Theme::dark();
        let block = format_tool_call(&t, "read", r#"{"path":"a.rs"}"#, Some("done"), false);
        assert!(block.contains(OK_MARKER), "no check mark in {block:?}");
        assert!(!block.contains(ERR_MARKER), "stray cross in {block:?}");
        assert!(block.contains("read"), "missing name in {block:?}");
        assert!(block.contains("a.rs"), "missing args in {block:?}");
        assert!(block.contains("done"), "missing result in {block:?}");
        assert!(block.contains("\x1b["), "unstyled block: {block:?}");
    }

    #[test]
    fn error_block_marks_a_cross_and_keeps_name_and_args() {
        let t = Theme::dark();
        let block = format_tool_call(&t, "bash", "cargo test", Some("boom: no such file"), true);
        assert!(block.contains(ERR_MARKER), "no cross in {block:?}");
        assert!(!block.contains(OK_MARKER), "stray check in {block:?}");
        assert!(block.contains("bash"), "missing name in {block:?}");
        assert!(block.contains("cargo test"), "missing args in {block:?}");
        assert!(block.contains("boom"), "missing result in {block:?}");
    }

    #[test]
    fn unfinished_call_has_only_the_header_line() {
        let t = Theme::dark();
        let block = format_tool_call(&t, "grep", "foo", None, false);
        assert_eq!(
            block.lines().count(),
            1,
            "unexpected extra lines: {block:?}"
        );
        assert!(block.contains(TOOL_MARKER), "missing marker in {block:?}");
        assert!(block.contains("grep"), "missing name in {block:?}");
        assert!(!block.contains(OK_MARKER), "stray check in {block:?}");
    }

    #[test]
    fn result_line_keeps_only_the_first_line_of_a_multiline_result() {
        let t = Theme::dark();
        let block = format_tool_call(
            &t,
            "read",
            "main.rs",
            Some("first line\nsecond line"),
            false,
        );
        assert!(block.contains("first line"), "{block:?}");
        assert!(!block.contains("second line"), "extra line kept: {block:?}");
        assert_eq!(block.lines().count(), 2, "{block:?}");
    }

    #[test]
    fn result_line_is_truncated_to_the_given_width() {
        let t = Theme::dark();
        let line = tool_result(&t, &"x".repeat(500), false, 20);
        let text = line.trim_end_matches('\n');
        assert!(visible_width(text) <= 20, "too wide: {text:?}");
        assert!(text.contains(OK_MARKER), "marker lost: {line:?}");
    }

    #[test]
    fn block_lines_never_exceed_the_terminal_width() {
        let t = Theme::dark();
        let block = format_tool_call(&t, "read", &"a".repeat(500), Some(&"b".repeat(500)), true);
        for line in block.lines() {
            assert!(
                visible_width(line) <= terminal_width(),
                "line too wide: {line:?}"
            );
        }
    }
}
