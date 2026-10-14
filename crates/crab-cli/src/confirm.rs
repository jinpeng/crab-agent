use crab_core::gate::{Confirmer, Decision, Prompt};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// `\r` (carriage return: cursor to column 0) followed by `\x1b[2K` (erase the
/// whole line). Wipes whatever currently occupies the line we are about to
/// write, without moving the cursor.
pub const CLEAR_LINE: &str = "\r\x1b[2K";

/// True while a confirmation prompt is on screen waiting for an answer.
///
/// The confirmer and the render task both write to stdout, and the renderer
/// keeps its status line on the *current* line. While a prompt is pending the
/// confirmer owns that line, so the renderer must not erase or redraw it — see
/// [`crate::render::render_events_with_status`]. The agent publishes no event
/// when it blocks on a confirmation (`RunState::WaitingConfirm` is never
/// signalled), so the confirmer itself is the only honest source of that fact.
static CONFIRM_PENDING: AtomicBool = AtomicBool::new(false);

/// Is a confirmation prompt currently waiting for an answer?
pub fn confirm_pending() -> bool {
    CONFIRM_PENDING.load(Ordering::SeqCst)
}

/// Clears the pending flag on every exit path out of the confirmer — a valid
/// answer, EOF, or a panic — so the renderer can never be left waiting forever.
struct PendingGuard;

impl Drop for PendingGuard {
    fn drop(&mut self) {
        CONFIRM_PENDING.store(false, Ordering::SeqCst);
    }
}

/// The exact bytes written for one confirmation prompt (the caller adds the
/// flush).
///
/// Starts with [`CLEAR_LINE`]: the renderer's status line may be sitting on the
/// current line, and printing the prompt on top of it would leave both
/// mangled. Clearing the line first means the prompt always appears
/// immediately, on a line of its own, no matter what the renderer last drew
/// there.
pub fn prompt_text(p: &Prompt) -> String {
    format!(
        "{CLEAR_LINE}\n\u{26a0} {} \u{2014} {} \nallow? [y]es/[n]o/[a]ll: ",
        p.tool, p.summary
    )
}

pub fn parse_answer(s: &str) -> Option<Decision> {
    match s.trim().to_lowercase().as_str() {
        "y" | "yes" => Some(Decision::Allow),
        "n" | "no" => Some(Decision::Deny),
        "a" | "all" => Some(Decision::AllowAll),
        _ => None,
    }
}

/// Terminal confirmer for interactive mode. Synchronous: when a tool call needs
/// confirmation the agent is mid-run and reedline is idle, so a blocking stdin read
/// is fine here.
pub fn terminal_confirmer() -> Confirmer {
    Arc::new(|p: Prompt| {
        CONFIRM_PENDING.store(true, Ordering::SeqCst);
        let _pending = PendingGuard;
        loop {
            // Clear the status line, print the prompt on its own fresh line and
            // flush *before* blocking on stdin: the question has to be on screen
            // while we wait for it, not only once the answer's newline arrives.
            let mut out = std::io::stdout();
            let _ = write!(out, "{}", prompt_text(&p));
            let _ = out.flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return Decision::Deny;
            }
            if let Some(d) = parse_answer(&line) {
                // read_line consumed the Enter's newline without echoing it, so the
                // cursor is still on the prompt line. Terminate that line so the
                // status line / next output starts fresh instead of appending after
                // the user's answer.
                println!();
                return d;
            }
            println!("please answer y, n, or a");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_core::gate::Decision;

    fn prompt() -> Prompt {
        Prompt {
            tool: "bash",
            summary: "rm -rf /tmp/x".into(),
        }
    }

    #[test]
    fn parses_confirmation_answer() {
        assert_eq!(parse_answer("y"), Some(Decision::Allow));
        assert_eq!(parse_answer("n"), Some(Decision::Deny));
        assert_eq!(parse_answer("a"), Some(Decision::AllowAll));
        assert_eq!(parse_answer("yes"), Some(Decision::Allow));
        assert_eq!(parse_answer("garbage"), None);
    }

    #[test]
    fn prompt_clears_the_status_line_before_printing() {
        let text = prompt_text(&prompt());
        // The status line the renderer last drew lives on the current line: the
        // prompt must wipe it first or it gets overwritten / shifted.
        assert!(
            text.starts_with(CLEAR_LINE),
            "prompt must clear the current line first: {text:?}"
        );
        // ...and the newline right after it puts the prompt on a line of its own.
        assert_eq!(
            &text[CLEAR_LINE.len()..CLEAR_LINE.len() + 1],
            "\n",
            "no fresh line after clearing: {text:?}"
        );
    }

    #[test]
    fn prompt_shows_the_tool_the_summary_and_the_answer_hint() {
        let text = prompt_text(&prompt());
        assert!(text.contains("bash"), "tool missing: {text:?}");
        assert!(text.contains("rm -rf /tmp/x"), "summary missing: {text:?}");
        assert!(
            text.contains("allow? [y]es/[n]o/[a]ll: "),
            "answer hint missing: {text:?}"
        );
    }
}
