use clap::Parser;

mod app;
mod metrics;
mod render;
mod serve;
mod status;
mod theme;
mod trace;
mod tui;
mod update;
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
    /// Print the fully built system prompt (base + AGENTS.md + lessons) and exit.
    /// Debug/acceptance knob: lets scripts assert what the model would see without
    /// a provider round trip.
    #[arg(long)]
    show_system_prompt: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Render the span tree of a session file (turn -> llm/tool calls).
    Trace {
        file: std::path::PathBuf,
        /// Structured JSON instead of the text tree.
        #[arg(long)]
        json: bool,
    },
    /// Update a managed install to the latest release (no-op for cargo installs).
    Update,
    /// Token/cost/latency report from the local metrics database.
    Metrics {
        /// Window in days (default 7).
        #[arg(default_value = "7")]
        days: u32,
        /// Structured JSON instead of the text report.
        #[arg(long)]
        json: bool,
    },
    /// Run the remote-control daemon (PWA + JSONL/WebSocket). Binds to a
    /// concrete local interface; a device token gates the page and socket.
    Serve {
        /// Interface IP to bind (loopback/LAN/tailnet). Refuses wildcards.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(long, default_value = "7391")]
        port: u16,
        /// Print a pairing URL + QR for the running/new serve and exit
        /// (subcommand of serve, not a separate top-level one).
        #[arg(long)]
        pair: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Commands::Trace { file, json }) = &cli.command {
        return trace::run(file, *json);
    }
    if matches!(cli.command, Some(Commands::Update)) {
        return update::run_update().await.map_err(|e| anyhow::anyhow!(e));
    }
    if let Some(Commands::Metrics { days, json }) = &cli.command {
        return metrics::report(*days, *json);
    }
    if let Some(Commands::Serve { host, port, pair }) = &cli.command {
        if *pair {
            return serve::print_pair(host, *port);
        }
        return serve::run(host, *port).await;
    }
    if let Some(prompt) = cli.print.clone() {
        return run_print(&cli, prompt).await;
    }
    if cli.show_system_prompt {
        // 只组装不运行：与 build_agent 同一条路径（build_system_prompt +
        // with_lessons），供验收脚本断言模型实际看到的 prompt。
        let cwd = std::env::current_dir()?;
        let skills = crab_core::skills::discover(&cwd, &std::env::home_dir().unwrap_or_default());
        let sp = app::with_lessons(
            app::build_system_prompt(&cwd, cli.system.clone(), &skills),
            &cwd,
        );
        println!("{sp}");
        return Ok(());
    }
    let cfg = app::AppConfig::from(&cli);
    tui::run(&cfg, cli.continue_).await
}

/// Best-effort metrics sink: an independent broadcast receiver (观测不改变系统).
/// Lives until the agent drops the channel; failures only warn.
fn spawn_metrics_sink(
    agent: &mut crab_core::agent::Agent,
    resolved: &crab_core::config::ResolvedProvider,
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let db_path = cwd.join(".crab").join("metrics.sqlite");
    let provider = resolved.provider_name.clone();
    let model = resolved.model.clone();
    let session_id = agent.session_id();
    let mut run_id = 0u64;
    let mut rx = agent.subscribe();
    tokio::spawn(async move {
        let Ok(db) = metrics::MetricsDb::open(&db_path) else {
            eprintln!("[crab] metrics: opening {db_path:?} failed; sink disabled");
            return;
        };
        let mut acc = metrics::RunAccumulator::new(metrics::load_pricing(), provider, model);
        while let Ok(ev) = rx.recv().await {
            if let Some(mut row) = acc.on_event(&ev) {
                row.session_id = session_id.clone();
                row.run_id = run_id;
                run_id += 1;
                if let Err(e) = db.record_turn(&row) {
                    eprintln!("[crab] metrics: record_turn failed: {e}");
                }
            }
        }
    });
}

async fn run_print(cli: &Cli, prompt: String) -> anyhow::Result<()> {
    let cfg = app::AppConfig::from(cli);
    // `startup.mcp_clients` holds the live MCP child processes (see `Startup`): it must
    // outlive the run below, so it is bound here rather than dropped inside `build_agent`.
    let startup = app::build_agent(&cfg, cli.continue_, None).await?;
    let mut agent = startup.agent;
    // Holds the live MCP child processes for the whole run (see app::build_agent).
    let mcp = startup.mcp_clients;
    spawn_metrics_sink(&mut agent, &startup.resolved);
    let rx = agent.subscribe();
    // Render until the channel closes, not until `Finished`: the run loop and the
    // provider→agent bridge task publish to the same channel, so a text delta can
    // still be enqueued after `Finished` (and after the renderer read it).
    let render = tokio::spawn(render::render_events_until_closed(rx));
    let reason = agent.run(prompt).await;
    // Dropping the agent releases the registry's Arc<McpClient> clones; only then
    // can shutdown_mcp take sole ownership and reap the MCP children while the
    // runtime is alive.
    drop(agent);
    app::shutdown_mcp(mcp).await;
    // Dropping the agent also drops the channel's last sender (`run` already
    // finished and awaited its own bridge sender), which lets the renderer drain
    // everything that was buffered and then exit. Await it on both paths — on the
    // error path too — so the process never returns before stdout is flushed.
    let _ = render.await;
    let reason = reason?;
    let _ = reason;
    Ok(())
}
