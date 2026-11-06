//! Full-screen terminal UI for interactive mode using ratatui.
//!
//! This module implements a proper alt-screen TUI with:
//! - Scrollable transcript of messages and tool calls
//! - Multi-line input with basic editing
//! - Status line showing model/state/tokens/context/cwd
//! - Permission confirmation as a modal overlay
//!
//! `--print` mode continues to use the simple streaming renderer in `render.rs`.

mod confirm;
mod input;
mod select;
mod state;
mod view;

use anyhow::Result;
use crab_core::agent::AgentEvent;
use crab_core::gate::{Decision, Prompt};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::Stdout;
use std::time::Duration;

use crate::app::{AppConfig, build_agent};
use confirm::ConfirmBridge;
pub use state::TuiState;

/// Run the interactive TUI.
pub async fn run(cfg: &AppConfig, continue_: bool) -> Result<()> {
    // Create confirmation bridge before building agent
    let bridge = ConfirmBridge::new();

    // Channel for UI events (agent events + confirm requests).
    // Unbounded: senders include tokio worker threads (agent forwarder, confirmer),
    // and a blocking send on a full bounded channel would stall the executor.
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();

    // Give the bridge a sender for confirm requests
    bridge.set_sender(ui_tx.clone());

    // Build agent with our confirmer
    let startup = build_agent(cfg, continue_, Some(bridge.confirmer())).await?;
    // `mcp_clients` must stay alive for the whole run (drop = kill the child
    // processes), so it is bound explicitly. `skills` feeds the TUI's /skill command.
    let crate::app::Startup {
        mut agent,
        resolved: provider,
        mcp_clients: _mcp,
        gate,
        config,
        skills,
        context_used,
        replay,
        usage_totals,
        cost,
    } = startup;
    // Best-effort metrics sink (independent broadcast receiver).
    crate::spawn_metrics_sink(&mut agent, &provider);

    // Provider profile names for the /model picker (sorted, mock last).
    let mut model_names: Vec<String> = config.providers.keys().cloned().collect();
    model_names.sort();
    model_names.push("mock".to_string());

    // Subscribe to agent events
    let agent_rx = agent.subscribe();

    // Refresh the git branch for the status line every 2s (only on change),
    // so the cwd row follows branch switches without blocking the draw thread.
    {
        let ui_tx = ui_tx.clone();
        let cwd = std::env::current_dir().unwrap_or_default();
        tokio::spawn(async move {
            let mut last: Option<String> = None;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.tick().await; // the interval's first tick fires immediately; skip it
            loop {
                tick.tick().await;
                let branch = crate::status::git_branch(&cwd);
                if branch != last {
                    last = branch.clone();
                    if ui_tx.send(UiEvent::BranchChanged(branch)).is_err() {
                        break; // TUI gone
                    }
                }
            }
        });
    }

    // Channel for UI commands (submit, abort, quit)
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<UiCommand>(16);

    // Initialize state
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut state = TuiState::new(
        format!("{}/{}", provider.provider_name, provider.model),
        cwd,
        provider.context_window,
        context_used,
    );
    // Replay the loaded session into the transcript so `--continue` shows the
    // conversation instead of starting from a blank screen.
    for msg in &replay {
        state.replay_message(msg);
    }
    // Seed the token counters from the log too — the status line resumes
    // where the session left off (↑↓R/W), rather than restarting at zero.
    state.status.seed_usage(usage_totals);
    // Same for the cost segment: replayed from the log, then folded live by
    // the pricer (same pricing as `crab metrics`).
    let mut pricer = crate::metrics::Pricer::new(&provider.model);
    if let Some(c0) = cost {
        pricer.seed(c0);
        state.status.set_cost(pricer.total());
    }
    state.pricer = Some(pricer);
    // Argument completion for the slash hint (/model, /skill:).
    state.model_names = model_names.clone();
    state.skills = skills.clone();
    state.yolo = gate.is_permissive();

    // Errors from agent runs go through the UI channel: printing to stderr
    // would corrupt the alt-screen.
    let err_tx = ui_tx.clone();

    // Forward agent events in a background task.
    // A broadcast receiver returns Err(Lagged) when it falls more than the channel
    // capacity behind — a long streaming reply produces hundreds of TextDeltas, so
    // lagging is NORMAL, not fatal. Skipping is safe here because the TUI state is
    // a fold over events and Finished is what matters; if we exited the loop on a
    // lag, Finished would be lost and the UI would lock up in "streaming" forever.
    let forward_tx = ui_tx.clone();
    let forward_handle = tokio::spawn(forward_agent_events(agent_rx, forward_tx));

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // A 0x0 (or sub-minimum) terminal renders an empty screen and looks frozen.
    // Bail with a clear message instead of silently drawing nothing.
    let (w, h) = terminal::size()?;
    if w < 10 || h < 5 {
        execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
        terminal::disable_raw_mode()?;
        anyhow::bail!("terminal too small ({w}x{h}); need at least 10x5");
    }

    // Guard to restore terminal on drop (panic-safe)
    let _guard = scopeguard::guard((), |_| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    });

    // Run the TUI loop in spawn_blocking (synchronous terminal I/O)
    let tick_rate = Duration::from_millis(50);
    let tui_cmd_tx = cmd_tx.clone();

    let mut tui_handle = tokio::task::spawn_blocking(move || {
        run_tui_loop(
            &mut terminal,
            state,
            ui_rx,
            tui_cmd_tx,
            tick_rate,
            model_names,
            skills,
        )
    });

    // Async command handler - runs agent runs and processes commands
    'outer: loop {
        tokio::select! {
            // Receive commands from TUI
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::Submit(first) => {
                        // Run the agent. Commands arriving mid-run are queued,
                        // not dropped — typing /model (or the next prompt)
                        // while the agent works must still take effect once
                        // the run finishes.
                        let mut next: Option<String> = Some(first);
                        while let Some(text) = next.take() {
                            let abort = agent.abort_token();
                            let mut pending: Vec<UiCommand> = Vec::new();
                            {
                                let run = agent.run(text);
                                tokio::pin!(run);
                                loop {
                                    tokio::select! {
                                        r = &mut run => {
                                            if let Err(e) = r {
                                                let _ = err_tx.send(UiEvent::Error(e.to_string()));
                                            }
                                            break;
                                        }
                                        Some(cmd) = cmd_rx.recv() => {
                                            if matches!(cmd, UiCommand::Abort) {
                                                abort.cancel();
                                            } else {
                                                pending.push(cmd);
                                            }
                                        }
                                        else => break,
                                    }
                                }
                            } // run dropped here: the &mut agent borrow ends
                            for cmd in pending {
                                match cmd {
                                    // A queued prompt runs next, in order.
                                    UiCommand::Submit(text) => next = Some(text),
                                    UiCommand::Quit => break 'outer,
                                    other => {
                                        handle_command(
                                            other, &mut agent, &gate, &config, &ui_tx, &err_tx,
                                            &mut cmd_rx,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    }
                    other => {
                        if handle_command(other, &mut agent, &gate, &config, &ui_tx, &err_tx, &mut cmd_rx).await
                        {
                            break;
                        }
                    }
                }
            }
            // TUI loop ended
            res = &mut tui_handle => {
                res??; // Propagate error from TUI loop
                break;
            }
            else => break,
        }
    }

    // Cleanup
    if std::env::var("CRAB_DEBUG_CMD").is_ok() {
        eprintln!("[dbg] async: outer loop broken, cleaning up");
    }
    forward_handle.abort();
    let _ = forward_handle.await;
    // Reap the MCP children while the runtime is alive (plain drop leaks them —
    // the /quit bug). The agent must go first: its tool registry holds Arc clones
    // of every client, so shutdown_mcp can only take sole ownership afterwards.
    drop(agent);
    crate::app::shutdown_mcp(_mcp).await;
    Ok(())
}

/// Forward agent events to the UI channel until the agent is dropped.
///
/// A broadcast receiver returns `Err(Lagged)` when it falls more than the channel
/// capacity behind — a long streaming reply produces hundreds of TextDeltas, so
/// lagging is NORMAL, not fatal. If we exited the loop on a lag, `Finished` would
/// be lost and the UI would lock up in "streaming" forever (the yolo-hang bug).
async fn forward_agent_events(
    mut agent_rx: tokio::sync::broadcast::Receiver<AgentEvent>,
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
) {
    loop {
        match agent_rx.recv().await {
            Ok(ev) => {
                if ui_tx.send(UiEvent::Agent(ev)).is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Surface the drop so a garbled transcript is explainable.
                let _ = ui_tx.send(UiEvent::Error(format!(
                    "[warn] dropped {n} events (UI fell behind)"
                )));
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Commands from TUI to async handler.
enum UiCommand {
    Submit(String),
    Abort,
    Quit,
    /// Toggle session-wide allow-all (yolo). TUI flips its own indicator.
    ToggleYolo,
    /// Switch to a named provider profile. TUI updates its label on success.
    SetModel(String),
    /// `/compact`: compact the conversation immediately.
    Compact,
    /// `/reflect`: distill lessons from this session, human-confirmed.
    Reflect,
    /// Run one user-typed `!` shell command locally (no permission gate —
    /// the user typed it themselves, like their own terminal).
    Shell(String),
    /// `/update`: self-update a managed install (async; result as a notice).
    Update,
}

/// Events to TUI from agent/async context.
enum UiEvent {
    Agent(AgentEvent),
    Error(String),
    Confirm {
        prompt: Prompt,
        reply: std::sync::mpsc::Sender<Decision>,
    },
    YoloChanged(bool),
    ModelChanged(String),
    /// Updated git branch for the status line (from the 2s refresher task).
    BranchChanged(Option<String>),
    /// A notice produced by the async side (e.g. "nothing to compact").
    Notice(String),
    /// A long-running slash command (/compact, /reflect) finished. TUI set
    /// Streaming at dispatch for immediate feedback; this restores Idle
    /// (unless a confirm modal or a real run owns the state now).
    CommandDone,
    /// A `!` shell command finished; show its output in the transcript.
    ShellDone {
        command: String,
        output: String,
        is_error: bool,
    },
}

/// Run one user-typed `!` shell command locally. No permission gate: the user
/// typed it themselves, which is the same trust level as their own terminal.
/// Output format mirrors the bash tool: head+tail truncation, [stderr], [exit N].
async fn run_shell_command(cmd: &str) -> (String, bool) {
    const TIMEOUT: Duration = Duration::from_secs(120);
    const HEAD_BYTES: usize = 30_000;
    const TAIL_BYTES: usize = 30_000;

    fn truncate(s: &str) -> String {
        if s.len() <= HEAD_BYTES + TAIL_BYTES {
            return s.to_string();
        }
        // Snap both cuts to char boundaries: byte-slicing a multi-byte char panics.
        let mut head_end = HEAD_BYTES;
        while !s.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = s.len() - TAIL_BYTES;
        while !s.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        format!(
            "{}\n... [omitted {} bytes] ...\n{}",
            &s[..head_end],
            s.len() - head_end - (s.len() - tail_start),
            &s[tail_start..]
        )
    }

    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .kill_on_drop(true) // dropping the future kills the process (abort-safe)
        .output();
    match tokio::time::timeout(TIMEOUT, child).await {
        Ok(Ok(out)) => {
            let mut text = truncate(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                text.push_str("\n[stderr]\n");
                text.push_str(&truncate(&String::from_utf8_lossy(&out.stderr)));
            }
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            text.push_str(&format!("\n[exit {code}]"));
            (text, !out.status.success())
        }
        Ok(Err(e)) => (format!("[spawn error: {e}]"), true),
        Err(_) => (
            format!("[timeout after {:?}: process killed]", TIMEOUT),
            true,
        ),
    }
}

/// Execute one non-Submit command. Returns true when the TUI should quit.
/// Shared by the idle loop and the post-run drain of mid-run commands.
async fn handle_command(
    cmd: UiCommand,
    agent: &mut crab_core::agent::Agent,
    gate: &crab_core::gate::PermissionGate,
    config: &crab_core::config::Config,
    ui_tx: &std::sync::mpsc::Sender<UiEvent>,
    err_tx: &std::sync::mpsc::Sender<UiEvent>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<UiCommand>,
) -> bool {
    match cmd {
        UiCommand::Abort => {
            // No-op if nothing running (mid-run Abort is handled inline).
            false
        }
        UiCommand::ToggleYolo => {
            let on = !gate.is_permissive();
            gate.set_allow_all(on);
            let _ = ui_tx.send(UiEvent::YoloChanged(on));
            false
        }
        UiCommand::SetModel(name) => {
            match crate::app::resolve_provider(config, Some(&name)) {
                Ok((new_provider, resolved)) => {
                    agent.set_provider(new_provider);
                    // allowlist 是 spawn 时的一阶约束，也是请求时的二阶约束：
                    // 切到带 allowlist 的 provider（如 Moonshot 系的 kimi）必须
                    // 同步过滤广告给模型的 MCP 工具，否则上个 profile spawn 的
                    // 大 schema server（如 rea 3.4MB/207 tools）会随请求发过去。
                    agent.set_mcp_allowlist(resolved.mcp_allowlist.clone());
                    let excluded = agent.excluded_mcp_servers();
                    if !excluded.is_empty() {
                        let _ = ui_tx.send(UiEvent::Notice(format!(
                            "{name}: not advertising tools from mcp servers: {} \
                             (still running; adjust mcp_allowlist to change)",
                            excluded.join(", ")
                        )));
                    }
                    let label = format!("{}/{}", resolved.provider_name, resolved.model);
                    let _ = ui_tx.send(UiEvent::ModelChanged(label));
                }
                Err(e) => {
                    let _ = err_tx.send(UiEvent::Error(format!("/model {name}: {e}")));
                }
            }
            false
        }
        UiCommand::Shell(cmd) => {
            // Off-loop like /update: a slow command must not freeze queued input.
            let tx = ui_tx.clone();
            tokio::spawn(async move {
                let (output, is_error) = run_shell_command(&cmd).await;
                let _ = tx.send(UiEvent::ShellDone {
                    command: cmd,
                    output,
                    is_error,
                });
            });
            false
        }
        UiCommand::Update => {
            // Runs off-loop: the download takes seconds and must
            // not freeze the input; the result comes back as a
            // notice bubble.
            let tx = ui_tx.clone();
            tokio::spawn(async move {
                let msg = match crate::update::update_once().await {
                    Ok(lines) => lines.join("\n"),
                    Err(e) => format!("/update failed: {e}"),
                };
                let _ = tx.send(UiEvent::Notice(msg));
            });
            false
        }
        UiCommand::Compact => {
            let op = agent.compact_now();
            tokio::pin!(op);
            // 与 Quit/Abort 竞争：/quit 立即退出（drop future 取消 LLM 调用），
            // Esc 取消本次压缩。其他命令 TUI 侧已锁定，不会到达。
            let report = tokio::select! {
                r = &mut op => Some(r),
                cmd = cmd_rx.recv() => {
                    if matches!(cmd, Some(UiCommand::Quit)) {
                        return true;
                    }
                    None
                }
            };
            let Some(report) = report else {
                let _ = ui_tx.send(UiEvent::Notice("compact cancelled".into()));
                let _ = ui_tx.send(UiEvent::CommandDone);
                return false;
            };
            if report.turns_affected == 0 {
                let _ = ui_tx.send(UiEvent::Notice("nothing to compact".into()));
            }
            // 非零情况由 AgentEvent::Compacted 处理器统一通知
            let _ = ui_tx.send(UiEvent::CommandDone);
            false
        }
        UiCommand::Reflect => {
            if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                eprintln!("[dbg] /reflect: handler entered");
            }
            // 块作用域让 future 在 select 后立即 drop——否则它对 agent 的
            // 不可变借用会挡住后面 append_lesson 的可变借用。
            let result = {
                let op = agent.propose_lessons();
                tokio::pin!(op);
                tokio::select! {
                    r = &mut op => Some(r),
                    cmd = cmd_rx.recv() => {
                        if matches!(cmd, Some(UiCommand::Quit)) {
                            return true;
                        }
                        None
                    }
                }
            };
            let Some(result) = result else {
                let _ = ui_tx.send(UiEvent::Notice("reflect cancelled".into()));
                let _ = ui_tx.send(UiEvent::CommandDone);
                return false;
            };
            match result {
                Ok(proposals) if proposals.is_empty() => {
                    let _ = ui_tx.send(UiEvent::Notice(
                        "nothing to reflect on (empty transcript or no lessons proposed)".into(),
                    ));
                }
                Ok(proposals) => {
                    let total = proposals.len();
                    let mut accepted = 0usize;
                    let mut accept_rest = false;
                    for (kind, text) in proposals {
                        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                            eprintln!("[dbg] /reflect: asking gate for [{kind}] {text}");
                        }
                        if !accept_rest {
                            // gate.ask 不碰 allow_all 闩：[a]ll 在这里的意思是
                            // "接受全部剩余提议"，绝不变成会话级自动放行。
                            match gate.ask(Prompt {
                                tool: "memory",
                                summary: format!("remember this lesson? [{kind}] {text}"),
                            }) {
                                Decision::Allow => {}
                                Decision::AllowAll => accept_rest = true,
                                Decision::Deny => continue,
                            }
                        }
                        if agent.append_lesson(kind, &text).is_some() {
                            accepted += 1;
                        }
                    }
                    if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                        eprintln!("[dbg] /reflect: done, {accepted}/{total}");
                    }
                    let _ = ui_tx.send(UiEvent::Notice(format!(
                        "remembered {accepted} lesson(s), skipped {}",
                        total - accepted
                    )));
                }
                Err(e) => {
                    let _ = err_tx.send(UiEvent::Error(format!("/reflect failed: {e}")));
                }
            }
            let _ = ui_tx.send(UiEvent::CommandDone);
            false
        }
        UiCommand::Submit(_) => false, // handled by the caller
        UiCommand::Quit => true,
    }
}

/// Synchronous TUI event loop.
fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut state: TuiState,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<UiCommand>,
    tick_rate: Duration,
    model_names: Vec<String>,
    skills: Vec<crab_core::skills::Skill>,
) -> Result<()> {
    loop {
        // Draw current state
        terminal.draw(|f| view::draw(f, &mut state))?;

        // Wait for the first event, then drain whatever is already queued so a burst
        // (mouse wheel, paste) costs one redraw instead of one per event.
        let mut ready = event::poll(tick_rate)?;
        while ready {
            if handle_event(event::read()?, &mut state, &cmd_tx, &model_names, &skills) {
                return Ok(());
            }
            ready = event::poll(Duration::ZERO)?;
        }

        // Drain UI events (non-blocking)
        while let Ok(ev) = ui_rx.try_recv() {
            match ev {
                UiEvent::Agent(ae) => state.apply_agent_event(ae),
                UiEvent::Error(msg) => state.show_error(msg),
                UiEvent::Confirm { prompt, reply } => {
                    if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                        eprintln!(
                            "[dbg] confirm modal shown: {} — {}",
                            prompt.tool, prompt.summary
                        );
                    }
                    state.begin_confirm(prompt, reply)
                }
                UiEvent::YoloChanged(on) => {
                    state.yolo = on;
                    state.notice(if on {
                        "yolo ON — all tool calls auto-approved"
                    } else {
                        "yolo OFF — tool calls need confirmation"
                    });
                }
                UiEvent::ModelChanged(label) => {
                    state.set_provider_label(label.clone());
                    state.status.set_provider_model(label.clone());
                    state.notice(format!("switched to {label}"));
                }
                UiEvent::BranchChanged(branch) => state.status.set_branch(branch),
                UiEvent::Notice(msg) => state.notice(msg),
                UiEvent::ShellDone {
                    command,
                    output,
                    is_error,
                } => state.push_shell_result(&command, output, is_error),
                UiEvent::CommandDone => {
                    // /reflect 可能弹过 confirm（WaitingConfirm）；若 modal 还在
                    // 就不动——状态归 modal 所有。只在 Working 时恢复 Idle。
                    if state.run_state == crate::status::RunState::Working {
                        state.set_run_state(crate::status::RunState::Idle);
                    }
                }
            }
        }

        if state.should_quit {
            if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                eprintln!("[dbg] tui: should_quit -> returning");
            }
            return Ok(());
        }
    }
}

/// Lines moved per mouse-wheel notch.
const WHEEL_LINES: usize = 3;

/// Handle one terminal event. Returns true when the TUI should exit.
fn handle_event(
    ev: Event,
    state: &mut TuiState,
    cmd_tx: &tokio::sync::mpsc::Sender<UiCommand>,
    model_names: &[String],
    skills: &[crab_core::skills::Skill],
) -> bool {
    match ev {
        Event::Key(key) => {
            // Model picker overlay takes precedence.
            if state.picker.is_some() {
                handle_picker_key(key.code, state, cmd_tx);
                return false;
            }
            if state.confirm.is_some() {
                // Route to confirm modal
                if let Some(decision) = handle_confirm_key(key.code, key.modifiers) {
                    state.answer_confirm(decision);
                }
                return false;
            }
            match handle_key(key.code, key.modifiers, state) {
                Some(Action::Submit(text)) => {
                    state.push_user(&text);
                    let _ = cmd_tx.blocking_send(UiCommand::Submit(text));
                }
                Some(Action::Shell(cmd)) => {
                    // Echo the command like a user message, but WITHOUT
                    // push_user's run-state side effect — a local shell command
                    // is not an agent run and must not grey out the input box.
                    state.bubbles.push(state::Bubble::User(format!("!{cmd}")));
                    state.scroll.scroll_to_bottom();
                    let _ = cmd_tx.blocking_send(UiCommand::Shell(cmd));
                }
                Some(Action::Abort) => {
                    let _ = cmd_tx.blocking_send(UiCommand::Abort);
                }
                Some(Action::Quit) => {
                    let _ = cmd_tx.blocking_send(UiCommand::Quit);
                    return true;
                }
                Some(Action::Slash(sc)) => {
                    dispatch_slash(sc, state, cmd_tx, model_names, skills);
                }
                None => {}
            }
        }
        // The wheel scrolls the transcript even while a modal or the agent is busy.
        // Press-drag-release over the transcript selects text and copies it on
        // release (pi-style copyOnSelect; the highlight lingers until the next press).
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => state.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => state.scroll_down(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => state.select_begin(m.column, m.row),
            MouseEventKind::Drag(MouseButton::Left) => state.select_update(m.column, m.row),
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(text) = state.select_finish() {
                    match select::copy_text(&text) {
                        Ok(how) => {
                            state.notice(format!("copied ({how}, {} chars)", text.chars().count()))
                        }
                        Err(e) => state.notice(format!("copy failed: {e}")),
                    }
                }
            }
            _ => {}
        },
        // ratatui re-measures on the next draw.
        _ => {}
    }
    false
}

/// Result of handling a key in normal mode.
#[derive(Debug, PartialEq)]
enum Action {
    Submit(String),
    /// A `!`-prefixed shell command to run locally.
    Shell(String),
    Abort,
    Quit,
    /// A slash command to dispatch to the async handler.
    Slash(state::SlashCommand),
}

/// Handle a key event in normal input mode.
fn handle_key(code: KeyCode, modifiers: KeyModifiers, state: &mut TuiState) -> Option<Action> {
    use KeyCode::*;

    // Ctrl+C is deliberately unbound: many terminals map it to copy, so crab
    // must not abort or quit on it (Esc aborts a running turn instead).
    if code == Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    // Slash-command hint (pi-style autocomplete) takes precedence while open:
    // ↑↓ navigate, Enter completes (consumed — press Enter again to run),
    // Esc dismisses. Synced first so the hint reflects the input before
    // this key. Esc closes the hint rather than aborting a run — a visible
    // popup means the user's intent is local.
    state.sync_slash_hint();
    if state.slash_hint.is_some() {
        match code {
            Up => {
                state.slash_hint_move(-1);
                return None;
            }
            Down => {
                state.slash_hint_move(1);
                return None;
            }
            Enter => {
                if state.slash_hint_complete() {
                    return None;
                }
            }
            Esc => {
                state.dismiss_slash_hint();
                return None;
            }
            _ => {}
        }
    }

    // Esc aborts the running turn (the pi-style interrupt). When idle it is
    // unbound here — pickers and the confirm modal handle their own Esc first.
    if code == Esc && state.is_running() {
        return Some(Action::Abort);
    }

    // Ctrl+D on empty input: quit
    if code == Char('d') && modifiers.contains(KeyModifiers::CONTROL) {
        if state.input.is_empty() {
            return Some(Action::Quit);
        }
        return None;
    }

    // Ctrl+A / Ctrl+E: start / end of the current input line
    if modifiers.contains(KeyModifiers::CONTROL) && !state.is_running() {
        match code {
            Char('a') => {
                state.input.move_home();
                return None;
            }
            Char('e') => {
                state.input.move_end();
                return None;
            }
            _ => {}
        }
    }

    // PageUp/PageDown: scroll transcript
    match code {
        PageUp => {
            state.scroll_up(state.scroll.page());
            return None;
        }
        PageDown => {
            state.scroll_down(state.scroll.page());
            return None;
        }
        Home => {
            state.scroll_to_top();
            return None;
        }
        End => {
            state.scroll_to_bottom();
            return None;
        }
        _ => {}
    }

    // Input editing keys. These stay live while the agent runs: keystrokes
    // buffer into the input box, Enter queues the prompt, and slash commands
    // (e.g. /model) are dispatched and queued by the async side. Esc (abort)
    // and scrolling are handled above.
    match code {
        Enter => {
            // Alt+Enter or Shift+Enter: insert newline (some terminals don't distinguish)
            if modifiers.contains(KeyModifiers::ALT) || modifiers.contains(KeyModifiers::SHIFT) {
                state.cancel_history_browsing();
                state.input.insert_char('\n');
                return None;
            }
            // slash 命令执行中（Working）：锁定提交——字符仍可输入，但 Enter
            // 不执行任何命令，/quit 系除外（退出永远即时）。agent run 的
            // mid-run 排队语义（Streaming）不受影响。
            if state.run_state == crate::status::RunState::Working {
                let t = state.input.text().trim();
                if !matches!(t, "/quit" | "/q" | "/exit") {
                    return None;
                }
            }
            // Enter: submit
            let text = state.input.take();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            // `!`-prefix: local shell command, green box. Not a prompt —
            // never reaches the model or the session log. A bare `!` is
            // neither executed nor recorded.
            if let Some(cmd) = trimmed.strip_prefix('!') {
                let cmd = cmd.trim();
                if cmd.is_empty() {
                    return None;
                }
                state.record_history(trimmed);
                return Some(Action::Shell(cmd.to_string()));
            }
            state.record_history(trimmed);
            if let Some(sc) = state::parse_slash(trimmed) {
                return Some(Action::Slash(sc));
            }
            Some(Action::Submit(text))
        }
        Backspace => {
            state.cancel_history_browsing();
            state.input.backspace();
            None
        }
        Delete => {
            state.cancel_history_browsing();
            state.input.delete();
            None
        }
        Left => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                state.input.move_word_left();
            } else {
                state.input.move_left();
            }
            None
        }
        Right => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                state.input.move_word_right();
            } else {
                state.input.move_right();
            }
            None
        }
        Up => {
            // History recall wins while there is anything to recall; with an
            // empty history (or a picker-less, browsing-less idle box) ↑
            // falls back to scrolling the transcript.
            if state.history_prev() {
                return None;
            }
            state.scroll_up(1);
            None
        }
        Down => {
            if state.history_next() {
                return None;
            }
            state.scroll_down(1);
            None
        }
        Char(c) => {
            state.cancel_history_browsing();
            state.input.insert_char(c);
            None
        }
        _ => None,
    }
}

/// Dispatch a parsed slash command. `/quit` exits the TUI; the rest either
/// mutate state directly (cheap) or forward to the async handler (model switch).
fn dispatch_slash(
    sc: state::SlashCommand,
    state: &mut TuiState,
    cmd_tx: &tokio::sync::mpsc::Sender<UiCommand>,
    model_names: &[String],
    skills: &[crab_core::skills::Skill],
) {
    match sc {
        state::SlashCommand::Quit => {
            let _ = cmd_tx.blocking_send(UiCommand::Quit);
            // The async loop breaks on Quit, but THIS loop must also end: the
            // runtime waits for spawn_blocking tasks at shutdown, so a TUI loop
            // left polling stdin hangs the process after /quit (the terminal
            // looks frozen with the MCP startup line still on screen).
            state.should_quit = true;
        }
        state::SlashCommand::Yolo => {
            let _ = cmd_tx.blocking_send(UiCommand::ToggleYolo);
        }
        state::SlashCommand::Model(Some(name)) => {
            let _ = cmd_tx.blocking_send(UiCommand::SetModel(name));
        }
        state::SlashCommand::Model(None) => {
            state.notice("usage: /model <name>  — or /models to pick from a list");
        }
        state::SlashCommand::PickModel => {
            let current = model_names
                .iter()
                .position(|n| state.provider_label.starts_with(n.as_str()))
                .unwrap_or(0);
            state.picker = Some(state::Picker::Model(state::ListPicker::new(
                model_names.to_vec(),
                current,
            )));
        }
        state::SlashCommand::Skill(name, args) => {
            if name.is_empty() {
                state.notice("usage: /skill:<name> [args]");
            } else if let Some(skill) = skills.iter().find(|s| s.name == name) {
                let msg = state::skill_message(skill, args.as_deref().unwrap_or(""));
                state.notice(format!("skill loaded: {name}"));
                let _ = cmd_tx.blocking_send(UiCommand::Submit(msg));
            } else {
                let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
                state.notice(format!(
                    "unknown skill '{name}'. Available: {}",
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    }
                ));
            }
        }
        state::SlashCommand::PickSkill => match state::skill_picker(skills) {
            Some(picker) => state.picker = Some(picker),
            None => state
                .notice("no skills found (looked in ~/.agents/skills and project .agents/skills)"),
        },
        state::SlashCommand::Update => {
            let _ = cmd_tx.blocking_send(UiCommand::Update);
        }
        state::SlashCommand::Compact => {
            if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                eprintln!("[dbg] /compact: sending UiCommand::Compact");
            }
            // 立即反馈：spinner + 灰化输入框，避免"没反应"的错觉。
            // 不吞键——后续命令照常排队，/quit 仍能即时退出。
            state.set_run_state(crate::status::RunState::Working);
            let _ = cmd_tx.blocking_send(UiCommand::Compact);
        }
        state::SlashCommand::Reflect => {
            if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                eprintln!("[dbg] /reflect: sending UiCommand::Reflect");
            }
            state.set_run_state(crate::status::RunState::Working);
            let _ = cmd_tx.blocking_send(UiCommand::Reflect);
        }
        state::SlashCommand::Unknown(name) => {
            state.notice(format!(
                "unknown command /{name}  (try /quit, /yolo, /model <name>, /skills, /skill:<name>, /compact)"
            ));
        }
    }
}

/// Handle a key while the model picker overlay is open.
fn handle_picker_key(
    code: KeyCode,
    state: &mut TuiState,
    cmd_tx: &tokio::sync::mpsc::Sender<UiCommand>,
) {
    use KeyCode::*;
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
    match code {
        Up | Char('k') => picker.move_up(),
        Down | Char('j') => picker.move_down(),
        Enter => {
            let selected = picker.selected();
            match picker {
                state::Picker::Skill { names, .. } => {
                    if let Some(name) = names.get(selected) {
                        // Skill chosen: close the picker and prefill the input box
                        // so the user types args and hits Enter (the normal /skill path).
                        let line = state::skill_slash_line(name);
                        state.picker = None;
                        state.input.clear();
                        state.input.insert_str(&line);
                    }
                }
                state::Picker::Model(l) => {
                    if let Some(name) = l.selected_name().map(str::to_string) {
                        state.picker = None;
                        let _ = cmd_tx.blocking_send(UiCommand::SetModel(name));
                    }
                }
            }
        }
        Esc | Char('q') => {
            state.picker = None;
        }
        _ => {}
    }
}

/// Handle a key in confirm modal. Returns Some(decision) if answered.
fn handle_confirm_key(code: KeyCode, _modifiers: KeyModifiers) -> Option<Decision> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::Deny),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::AllowAll),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the yolo-hang: when a long streaming reply overflows the
    /// broadcast channel, the forwarder must skip the lagged events and keep going —
    /// exiting the loop would lose `Finished` and lock the UI in "streaming" forever.
    // Multi-threaded: draining the std::sync::mpsc receiver blocks the calling
    // thread, so the forwarder needs another worker thread to make progress.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwarder_survives_broadcast_lag() {
        // Tiny capacity forces a lag: 10 events pending before the forwarder polls.
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        for i in 0..10 {
            tx.send(AgentEvent::TextDelta(format!("delta-{i}")))
                .unwrap();
        }
        tx.send(AgentEvent::Finished {
            reason: crab_core::types::StopReason::EndTurn,
        })
        .unwrap();

        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let handle = tokio::spawn(forward_agent_events(rx, ui_tx));

        let mut saw_lag_notice = false;
        let mut saw_finished = false;
        // Drain until Finished arrives or the channel closes (tx dropped below).
        for ev in ui_rx.iter() {
            match ev {
                UiEvent::Error(m) if m.contains("dropped") => saw_lag_notice = true,
                UiEvent::Agent(AgentEvent::Finished { .. }) => {
                    saw_finished = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_lag_notice, "expected a lag notice");
        assert!(saw_finished, "Finished must not be lost to a lag");

        drop(tx);
        handle.await.unwrap();
    }
    #[test]
    fn slash_commands_enter_working_state_and_quit_still_exits() {
        use crate::status::RunState;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        // /reflect 和 /compact 立即进入 Working（spinner + 输入框灰化）。
        for cmd in ["/reflect", "/compact"] {
            s.run_state = RunState::Idle;
            s.input.insert_str(cmd);
            let action = handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut s);
            let Some(Action::Slash(sc)) = action else {
                panic!("{cmd}: expected slash action, got {action:?}");
            };
            dispatch_slash(sc, &mut s, &tx, &[], &[]);
            assert_eq!(
                s.run_state,
                RunState::Working,
                "{cmd}: must enter working state immediately"
            );
            // 命令已排队，TUI 不吞键。
            assert!(rx.try_recv().is_ok(), "{cmd}: command must be queued");
        }
        // Working 中其他命令的 Enter 被吞（不提交、输入保留）。
        s.input.insert_str("/compact");
        assert_eq!(handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut s), None);
        assert_eq!(
            s.input.text(),
            "/compact",
            "input is preserved, not submitted"
        );
        // /quit 在 Working 状态仍然即时退出（TUI 侧 return true，不等 handler）。
        s.input.take();
        s.input.insert_str("/quit");
        let Some(Action::Slash(state::SlashCommand::Quit)) =
            handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut s)
        else {
            panic!("quit must still dispatch while working");
        };
    }

    #[test]
    fn command_done_restores_idle_but_not_during_confirm() {
        use crate::status::RunState;
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        // Working → Idle
        s.run_state = RunState::Working;
        if s.run_state == RunState::Working {
            s.set_run_state(RunState::Idle);
        }
        assert_eq!(s.run_state, RunState::Idle);
        // confirm 进行中不被 CommandDone 覆盖
        let (reply, _) = std::sync::mpsc::channel();
        s.begin_confirm(
            crab_core::gate::Prompt {
                tool: "memory",
                summary: "x".into(),
            },
            reply,
        );
        assert_eq!(s.run_state, RunState::WaitingConfirm);
        assert!(s.confirm.is_some());
    }

    #[test]
    fn esc_aborts_a_running_turn() {
        use crate::status::RunState;
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.run_state = RunState::Streaming;
        assert_eq!(
            handle_key(KeyCode::Esc, KeyModifiers::NONE, &mut s),
            Some(Action::Abort)
        );
    }

    #[test]
    fn esc_is_a_noop_when_idle() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        assert_eq!(handle_key(KeyCode::Esc, KeyModifiers::NONE, &mut s), None);
    }

    #[test]
    fn ctrl_c_does_nothing_running_or_idle() {
        use crate::status::RunState;
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        // Running: must NOT abort — the key is left for the terminal's copy habit.
        s.run_state = RunState::Streaming;
        assert_eq!(
            handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, &mut s),
            None
        );
        // Idle: must NOT clear the input either.
        s.run_state = RunState::Idle;
        s.input.insert_str("hello");
        assert_eq!(
            handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, &mut s),
            None
        );
        assert_eq!(s.input.text(), "hello");
    }

    /// /quit must end the TUI loop itself, not just the async command loop:
    /// the runtime waits for spawn_blocking tasks at shutdown, so a TUI loop
    /// left polling stdin would hang the process (the /quit freeze).
    #[test]
    fn bang_prefix_dispatches_shell_action() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.input.insert_str("!ls -la");
        let action = handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut s);
        assert_eq!(action, Some(Action::Shell("ls -la".into())));
        assert!(s.input.is_empty(), "buffer consumed on submit");
        assert_eq!(s.history, vec!["!ls -la".to_string()]);
        // 提交也清掉浏览状态。
        assert_eq!(s.hist_index, None);
    }

    #[test]
    fn bare_bang_submits_nothing() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.input.insert_str("!");
        assert_eq!(handle_key(KeyCode::Enter, KeyModifiers::NONE, &mut s), None);
        assert!(s.input.is_empty(), "consumed, not executed");
        assert!(s.history.is_empty(), "an empty command is not history");
    }

    #[test]
    fn up_recalls_history_newest_first_down_restores_draft() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.record_history("first prompt");
        s.record_history("!echo hi");
        // 空输入按 ↑：最新一条先进框。
        assert!(handle_key(KeyCode::Up, KeyModifiers::NONE, &mut s).is_none());
        assert_eq!(s.input.text(), "!echo hi");
        // 再 ↑：更旧的一条。
        assert!(handle_key(KeyCode::Up, KeyModifiers::NONE, &mut s).is_none());
        assert_eq!(s.input.text(), "first prompt");
        // 到顶后再 ↑ 不动。
        assert!(handle_key(KeyCode::Up, KeyModifiers::NONE, &mut s).is_none());
        assert_eq!(s.input.text(), "first prompt");
        // ↓ 回到较新一条，再 ↓ 越界 → 恢复浏览前保存的草稿（空）。
        assert!(handle_key(KeyCode::Down, KeyModifiers::NONE, &mut s).is_none());
        assert_eq!(s.input.text(), "!echo hi");
        assert!(handle_key(KeyCode::Down, KeyModifiers::NONE, &mut s).is_none());
        assert_eq!(s.input.text(), "", "draft (empty) restored past the newest entry");
        // 不再浏览时 ↓ 回落到滚动语义（返回值仍是 None，但没有 panic）。
        assert!(handle_key(KeyCode::Down, KeyModifiers::NONE, &mut s).is_none());
    }

    #[test]
    fn typing_while_browsing_cancels_recall() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.record_history("earlier");
        s.input.insert_str("draft text");
        handle_key(KeyCode::Up, KeyModifiers::NONE, &mut s);
        assert_eq!(s.input.text(), "earlier");
        // 编辑召回的内容：浏览结束，文本保留可改。
        handle_key(KeyCode::Char('!'), KeyModifiers::NONE, &mut s);
        assert_eq!(s.hist_index, None);
        assert_eq!(s.input.text(), "earlier!");
        // 再 ↑ 以当前文本为新草稿重新召回。
        handle_key(KeyCode::Up, KeyModifiers::NONE, &mut s);
        assert_eq!(s.input.text(), "earlier");
        handle_key(KeyCode::Down, KeyModifiers::NONE, &mut s);
        assert_eq!(s.input.text(), "earlier!");
    }

    #[test]
    fn consecutive_duplicate_history_entries_collapse() {
        let mut s = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        s.record_history("same");
        s.record_history("same");
        s.record_history("other");
        s.record_history("same");
        assert_eq!(s.history, vec!["same", "other", "same"]);
    }

    #[tokio::test]
    async fn shell_command_captures_output_and_exit_code() {
        let (out, failed) = run_shell_command("echo hello-crab").await;
        assert!(out.contains("hello-crab"), "{out}");
        assert!(!failed, "exit 0 is not an error: {out}");

        let (out, failed) = run_shell_command("echo oops 1>&2; exit 3").await;
        assert!(out.contains("oops"), "stderr captured: {out}");
        assert!(out.contains("[exit 3]"), "exit code recorded: {out}");
        assert!(failed, "non-zero exit is an error");
    }

    #[test]
    fn slash_quit_sets_should_quit() {
        let mut state = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        dispatch_slash(state::SlashCommand::Quit, &mut state, &tx, &[], &[]);
        assert!(state.should_quit);
    }
}
