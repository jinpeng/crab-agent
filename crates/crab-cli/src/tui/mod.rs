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
mod state;
mod view;

use anyhow::Result;
use crab_core::agent::AgentEvent;
use crab_core::gate::{Decision, Prompt};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;
use std::time::Duration;

use crate::app::{build_agent, AppConfig};
use confirm::ConfirmBridge;
pub use state::TuiState;

/// Run the interactive TUI.
pub async fn run(cfg: &AppConfig, continue_: bool) -> Result<()> {
    // Create confirmation bridge before building agent
    let bridge = ConfirmBridge::new();

    // Channel for UI events (agent events + confirm requests)
    let (ui_tx, ui_rx) = std::sync::mpsc::sync_channel::<UiEvent>(64);

    // Give the bridge a sender for confirm requests
    bridge.set_sender(ui_tx.clone());

    // Build agent with our confirmer
    let (mut agent, _sys, provider, _mcp) =
        build_agent(cfg, continue_, Some(bridge.confirmer())).await?;

    // Subscribe to agent events
    let mut agent_rx = agent.subscribe();

    // Channel for UI commands (submit, abort, quit)
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<UiCommand>(16);

    // Initialize state
    let cwd = std::env::current_dir().unwrap_or_default();
    let state = TuiState::new(
        format!("{}/{}", provider.provider_name, provider.model),
        cwd,
        provider.context_window,
    );

    // Errors from agent runs go through the UI channel: printing to stderr
    // would corrupt the alt-screen.
    let err_tx = ui_tx.clone();

    // Forward agent events in a background task
    let forward_tx = ui_tx.clone();
    let forward_handle = tokio::spawn(async move {
        while let Ok(ev) = agent_rx.recv().await {
            if forward_tx.send(UiEvent::Agent(ev)).is_err() {
                break;
            }
        }
    });

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Guard to restore terminal on drop (panic-safe)
    let _guard = scopeguard::guard((), |_| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
    });

    // Run the TUI loop in spawn_blocking (synchronous terminal I/O)
    let tick_rate = Duration::from_millis(50);
    let tui_cmd_tx = cmd_tx.clone();

    let mut tui_handle = tokio::task::spawn_blocking(move || {
        run_tui_loop(&mut terminal, state, ui_rx, tui_cmd_tx, tick_rate)
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
    Ok(())
}

/// Commands from TUI to async handler.
enum UiCommand {
    Submit(String),
    Abort,
    Quit,
}

/// Events to TUI from agent/async context.
enum UiEvent {
    Agent(AgentEvent),
    Error(String),
    Confirm {
        prompt: Prompt,
        reply: std::sync::mpsc::Sender<Decision>,
    },
}

/// Synchronous TUI event loop.
fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut state: TuiState,
    ui_rx: std::sync::mpsc::Receiver<UiEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<UiCommand>,
    tick_rate: Duration,
) -> Result<()> {
    loop {
        // Draw current state
        terminal.draw(|f| view::draw(f, &mut state))?;

        // Wait for the first event, then drain whatever is already queued so a burst
        // (mouse wheel, paste) costs one redraw instead of one per event.
        let mut ready = event::poll(tick_rate)?;
        while ready {
            if handle_event(event::read()?, &mut state, &cmd_tx) {
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
) -> bool {
    match ev {
        Event::Key(key) => {
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
                None => {}
            }
        }
        // The wheel scrolls the transcript even while a modal or the agent is busy.
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => state.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => state.scroll_down(WHEEL_LINES),
            _ => {}
        },
        // ratatui re-measures on the next draw.
        _ => {}
    }
    false
}

/// Result of handling a key in normal mode.
enum Action {
    Submit(String),
    Abort,
    Quit,
}

/// Handle a key event in normal input mode.
fn handle_key(code: KeyCode, modifiers: KeyModifiers, state: &mut TuiState) -> Option<Action> {
    use KeyCode::*;

    // Ctrl+C: abort if running, otherwise clear input
    if code == Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        if state.is_running() {
            return Some(Action::Abort);
        } else {
            state.input.clear();
            return None;
        }
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
            if text.trim().is_empty() {
                return None;
            }
            // Check for slash commands
            let trimmed = text.trim();
            if trimmed == "/quit" || trimmed == "/exit" {
                return Some(Action::Quit);
            }
            return Some(Action::Submit(text));
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

/// Handle a key in confirm modal. Returns Some(decision) if answered.
fn handle_confirm_key(code: KeyCode, _modifiers: KeyModifiers) -> Option<Decision> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Decision::Allow),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::Deny),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::AllowAll),
        _ => None,
    }
}
