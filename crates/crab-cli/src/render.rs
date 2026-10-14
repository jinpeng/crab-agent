use crab_core::agent::AgentEvent;

/// Stream agent events to stdout (crossterm-free plain streaming; no alt-screen, Q10).
/// TextDelta prints inline without a newline; tool calls are marked; Finished ends the line.
pub async fn render_events(mut rx: tokio::sync::broadcast::Receiver<AgentEvent>) {
    use std::io::Write;
    let mut out = std::io::stdout();
    loop {
        match rx.recv().await {
            Ok(AgentEvent::TextDelta(s)) => {
                print!("{s}");
                let _ = out.flush();
            }
            Ok(AgentEvent::ToolCallStarted { name, .. }) => {
                print!("\n[tool: {name}]");
                let _ = out.flush();
            }
            Ok(AgentEvent::ToolCallFinished { is_error, .. }) => {
                if is_error {
                    print!(" [error]");
                    let _ = out.flush();
                }
            }
            Ok(AgentEvent::Finished { .. }) => {
                println!();
                break;
            }
            Ok(_) => {}
            // Channel closed or lagged: stop rendering.
            Err(_) => break,
        }
    }
}
