use crate::app::{AppConfig, build_agent};
use crate::render::render_events;
use anyhow::Result;
use reedline::{DefaultPrompt, Reedline, Signal};

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
    let (mut agent, _sys) = build_agent(cfg, continue_)?;
    let mut rl = Reedline::create();
    let prompt = DefaultPrompt::default();
    println!("crab interactive. /quit to exit, Ctrl-C aborts the current run.");

    loop {
        // reedline read_line is blocking → spawn_blocking. Reedline isn't Send-safe to
        // hold across await, so move it in and back out each iteration.
        let (returned_rl, sig) = tokio::task::spawn_blocking({
            let mut rl_moved = std::mem::replace(&mut rl, Reedline::create());
            let prompt = prompt.clone();
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
                    let render = tokio::spawn(render_events(rx));
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
}
