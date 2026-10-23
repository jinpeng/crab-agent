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
    loop {
        tokio::select! {
            // Receive commands from TUI
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::Submit(text) => {
                        // Run the agent
                        let abort = agent.abort_token();
                        let run = agent.run(text);
                        tokio::pin!(run);

                        // Wait for completion or abort
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
                                    }
                                    // Ignore other commands while running
                                }
                                else => break,
                            }
                        }
                    }
                    UiCommand::Abort => {
                        // No-op if nothing running
                    }
                    UiCommand::ToggleYolo => {
                        let on = !gate.is_permissive();
                        gate.set_allow_all(on);
                        let _ = ui_tx.send(UiEvent::YoloChanged(on));
                    }
                    UiCommand::SetModel(name) => {
                        match crate::app::resolve_provider(&config, Some(&name)) {
                            Ok((new_provider, resolved)) => {
                                agent.set_provider(new_provider);
                                let label = format!("{}/{}", resolved.provider_name, resolved.model);
                                let _ = ui_tx.send(UiEvent::ModelChanged(label));
                            }
                            Err(e) => {
                                let _ = err_tx.send(UiEvent::Error(format!("/model {name}: {e}")));
                            }
                        }
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
                    }
                    UiCommand::Compact => {
                        let report = agent.compact_now().await;
                        if report.turns_affected == 0 {
                            let _ = ui_tx.send(UiEvent::Notice("nothing to compact".into()));
                        }
                        // 非零情况由 AgentEvent::Compacted 处理器统一通知
                    }
                    UiCommand::Quit => break,
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
                UiEvent::Confirm { prompt, reply } => state.begin_confirm(prompt, reply),
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
            }
        }

        if state.should_quit {
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

    // If agent is running, don't process input keys
    if state.is_running() {
        return None;
    }

    // Input editing keys
    match code {
        Enter => {
            // Alt+Enter or Shift+Enter: insert newline (some terminals don't distinguish)
            if modifiers.contains(KeyModifiers::ALT) || modifiers.contains(KeyModifiers::SHIFT) {
                state.input.insert_char('\n');
                return None;
            }
            // Enter: submit
            let text = state.input.take();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Some(sc) = state::parse_slash(trimmed) {
                return Some(Action::Slash(sc));
            }
            Some(Action::Submit(text))
        }
        Backspace => {
            state.input.backspace();
            None
        }
        Delete => {
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
            // TODO: command history
            state.scroll_up(1);
            None
        }
        Down => {
            state.scroll_down(1);
            None
        }
        Char(c) => {
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
            let _ = cmd_tx.blocking_send(UiCommand::Compact);
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
    fn slash_quit_sets_should_quit() {
        let mut state = state::TuiState::new("m".into(), std::path::PathBuf::from("/"), None, None);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        dispatch_slash(state::SlashCommand::Quit, &mut state, &tx, &[], &[]);
        assert!(state.should_quit);
    }
}
