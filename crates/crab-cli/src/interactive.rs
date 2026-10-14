use crate::app::{AppConfig, build_agent};
use crate::render::render_events_with_status;
use crate::status::StatusLine;
use anyhow::Result;
use reedline::{Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal};
use std::borrow::Cow;

/// Minimal prompt: a plain `crab> `, with no cwd, date, or mode glyphs in the way.
#[derive(Clone, Copy, Debug, Default)]
pub struct CrabPrompt;

impl Prompt for CrabPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("crab")
    }

    // Deliberately mode-independent: vi/helix mode changes must not restyle the
    // indicator, so `crab> ` stays byte-identical in every edit mode.
    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("··· ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("search: ")
    }
}

#[derive(Debug, PartialEq)]
pub enum Input {
    Quit,
    Skip,
    Message(String),
}

pub fn classify(s: &str) -> Input {
    let t = s.trim();
    match t {
        "/quit" | "/exit" => Input::Quit,
        "" => Input::Skip,
        _ => Input::Message(t.to_string()),
    }
}

pub async fn run_interactive(cfg: &AppConfig, continue_: bool) -> Result<()> {
    let (mut agent, _sys, provider) = build_agent(cfg, continue_)?;
    let mut rl = Reedline::create();
    let prompt = CrabPrompt;
    println!("crab interactive. /quit to exit, Ctrl-C aborts the current run.");

    loop {
        // reedline read_line is blocking → spawn_blocking. Reedline isn't Send-safe to
        // hold across await, so move it in and back out each iteration.
        let (returned_rl, sig) = tokio::task::spawn_blocking({
            let mut rl_moved = std::mem::replace(&mut rl, Reedline::create());
            // `CrabPrompt` is `Copy`, so the closure takes a snapshot and the
            // outer `prompt` stays usable for the next iteration.
            move || {
                let sig = rl_moved.read_line(&prompt);
                (rl_moved, sig)
            }
        })
        .await?; // JoinError; sig itself is a Result handled by the match below
        rl = returned_rl;

        match sig {
            Ok(Signal::Success(text)) => match classify(&text) {
                Input::Quit => break,
                Input::Skip => continue,
                Input::Message(m) => {
                    let rx = agent.subscribe();
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let status = StatusLine::new(
                        format!("{}/{}", provider.provider_name, provider.model),
                        cwd,
                    );
                    let mut status = status;
                    status.set_context(None, provider.context_window);
                    let render = tokio::spawn(render_events_with_status(rx, status));
                    let abort = agent.abort_token();
                    let run = agent.run(m);
                    tokio::pin!(run);
                    tokio::select! {
                        r = &mut run => { let _ = r?; }
                        _ = tokio::signal::ctrl_c() => {
                            abort.cancel();
                            let _ = run.await;
                            println!("\n[aborted]");
                        }
                    }
                    let _ = render.await;
                }
            },
            // In-editor Ctrl-C: clear the line, keep going (NOT an agent abort).
            Ok(Signal::CtrlC) => continue,
            // Ctrl-D / EOF: exit the session.
            Ok(Signal::CtrlD) => break,
            _ => continue,
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_input() {
        assert_eq!(classify("/quit"), Input::Quit);
        assert_eq!(classify("/exit"), Input::Quit);
        assert_eq!(classify(""), Input::Skip);
        assert_eq!(classify("   "), Input::Skip);
        assert_eq!(classify("hello"), Input::Message("hello".into()));
    }

    #[test]
    fn prompt_renders_minimal_crab_indicator() {
        let prompt = CrabPrompt;
        assert_eq!(prompt.render_prompt_left(), "crab");
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Default),
            "> "
        );
        assert_eq!(prompt.render_prompt_right(), "");
        assert_eq!(prompt.render_prompt_multiline_indicator(), "··· ");
        assert_eq!(
            prompt.render_prompt_history_search_indicator(PromptHistorySearch::new(
                reedline::PromptHistorySearchStatus::Passing,
                "x".into(),
            )),
            "search: "
        );
        // The visible prompt is exactly `crab> `.
        let rendered = format!(
            "{}{}",
            prompt.render_prompt_left(),
            prompt.render_prompt_indicator(PromptEditMode::Default)
        );
        assert_eq!(rendered, "crab> ");
    }
}
