use clap::Parser;

mod app;
mod confirm;
mod interactive;
mod render;
// TODO: used by interactive.rs once it renders the bottom status line;
// until then, silence dead_code.
#[allow(dead_code)]
mod status;
// TODO: used by render.rs once it consumes the theme; until then, silence dead_code.
#[allow(dead_code)]
mod theme;
// TODO: used by render.rs once it truncates styled output to the terminal width;
// until then, silence dead_code.
#[allow(dead_code)]
mod width;

#[derive(Parser)]
#[command(name = "crab", version, about = "Rust coding agent")]
pub struct Cli {
    /// Run once non-interactively and print the final response
    #[arg(long, short = 'p')]
    print: Option<String>,
    /// Continue the most recent session in .crab/sessions
    #[arg(long, short = 'c')]
    continue_: bool,
    /// Allow all tool calls without confirmation
    #[arg(long)]
    yolo: bool,
    /// Provider profile name from ~/.crab/config.toml (or "mock" for debugging).
    /// Omit to use the config's default_provider.
    #[arg(long)]
    provider: Option<String>,
    /// Extra system prompt appended after base + AGENTS.md
    #[arg(long)]
    system: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(prompt) = cli.print.clone() {
        return run_print(&cli, prompt).await;
    }
    let cfg = app::AppConfig::from(&cli);
    interactive::run_interactive(&cfg, cli.continue_).await
}

async fn run_print(cli: &Cli, prompt: String) -> anyhow::Result<()> {
    let cfg = app::AppConfig::from(cli);
    // `_mcp` holds the live MCP child processes (see `build_agent`): it must outlive
    // the run below, so it is bound here rather than dropped inside `build_agent`.
    let (mut agent, _sys, _provider, _mcp) = app::build_agent(&cfg, cli.continue_).await?;
    let rx = agent.subscribe();
    // Render until the channel closes, not until `Finished`: the run loop and the
    // provider→agent bridge task publish to the same channel, so a text delta can
    // still be enqueued after `Finished` (and after the renderer read it).
    let render = tokio::spawn(render::render_events_until_closed(rx));
    let reason = agent.run(prompt).await;
    // Dropping the agent drops the channel's last sender (`run` already finished and
    // awaited its own bridge sender), which lets the renderer drain everything that
    // was buffered and then exit. Await it on both paths — on the error path too —
    // so the process never returns before stdout is flushed.
    drop(agent);
    let _ = render.await;
    let reason = reason?;
    let _ = reason;
    Ok(())
}
