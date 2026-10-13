use clap::Parser;

mod app;
mod render;

#[derive(Parser)]
#[command(name = "crab", version, about = "Rust coding agent")]
struct Cli {
    /// Run once non-interactively and print the final response
    #[arg(long, short = 'p')]
    print: Option<String>,
    /// Continue the most recent session in .crab/sessions
    #[arg(long, short = 'c')]
    continue_: bool,
    /// Allow all tool calls without confirmation
    #[arg(long)]
    yolo: bool,
    /// Provider: "openai" (real) or "mock" (scripted, for debugging)
    #[arg(long, default_value = "openai")]
    provider: String,
    /// Extra system prompt appended after base + AGENTS.md
    #[arg(long)]
    system: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    println!(
        "crab: print={:?} continue={} yolo={} provider={}",
        cli.print, cli.continue_, cli.yolo, cli.provider
    );
}
