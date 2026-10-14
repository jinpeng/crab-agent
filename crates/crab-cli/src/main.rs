use clap::Parser;

mod app;
mod confirm;
mod interactive;
mod render;
// TODO: used by render.rs once it consumes the theme; until then, silence dead_code.
#[allow(dead_code)]
mod theme;

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
    let (mut agent, _sys) = app::build_agent(&cfg, cli.continue_)?;
    let rx = agent.subscribe();
    let render = tokio::spawn(render::render_events(rx));
    let reason = agent.run(prompt).await?;
    drop(agent);
    let _ = render.await;
    let _ = reason;
    Ok(())
}
