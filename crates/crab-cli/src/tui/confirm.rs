//! Bridge between synchronous Confirmer and async TUI.

use crab_core::gate::{Confirmer, Decision, Prompt};
use std::sync::{Arc, Mutex};

/// Shared state for confirm bridge.
///
/// The Confirmer closure captures an Arc<ConfirmBridgeInner> and uses it
/// to communicate with the TUI event loop via sync channels.
#[derive(Debug)]
struct ConfirmBridgeInner {
    sender: Mutex<Option<std::sync::mpsc::SyncSender<super::UiEvent>>>,
}

impl ConfirmBridgeInner {
    fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    fn set_sender(&self, tx: std::sync::mpsc::SyncSender<super::UiEvent>) {
        *self.sender.lock().unwrap() = Some(tx);
    }

    fn confirm(&self, prompt: Prompt) -> Decision {
        let sender = self.sender.lock().unwrap();
        if let Some(tx) = sender.as_ref() {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Decision>();
            let event = super::UiEvent::Confirm {
                prompt,
                reply: reply_tx,
            };
            if tx.send(event).is_ok() {
                // Block waiting for response from TUI
                return reply_rx.recv().unwrap_or(Decision::Deny);
            }
        }
        Decision::Deny
    }
}

/// Bridge that allows the synchronous Confirmer to communicate with the TUI.
pub struct ConfirmBridge {
    inner: Arc<ConfirmBridgeInner>,
}

impl ConfirmBridge {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ConfirmBridgeInner::new()),
        }
    }

    /// Set the sender for confirm requests (called after creation).
    pub fn set_sender(&self, tx: std::sync::mpsc::SyncSender<super::UiEvent>) {
        self.inner.set_sender(tx);
    }

    /// Create a Confirmer that routes through this bridge.
    pub fn confirmer(&self) -> Confirmer {
        let inner = self.inner.clone();
        Arc::new(move |prompt: Prompt| inner.confirm(prompt))
    }
}

impl Default for ConfirmBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConfirmBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmBridge").finish()
    }
}
