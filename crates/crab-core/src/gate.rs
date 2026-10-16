use crate::tool::ToolError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A dangerous operation awaiting confirmation.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub tool: &'static str,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    AllowAll, // session-wide "allow all from now on"
}

pub type Confirmer = Arc<dyn Fn(Prompt) -> Decision + Send + Sync>;

/// Permission gate (Q9): read ops pass through, write/net/delete go through the
/// confirmer. Explicitly NOT a security boundary — see DESIGN §6.
#[derive(Clone)]
pub struct PermissionGate {
    confirmer: Confirmer,
    yolo: bool,
    allow_all: Arc<AtomicBool>,
}

impl PermissionGate {
    pub fn yolo() -> Self {
        Self {
            confirmer: Arc::new(|_| Decision::Allow),
            yolo: true,
            allow_all: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn auto_approve() -> Self {
        Self {
            confirmer: Arc::new(|_| Decision::Allow),
            yolo: false,
            allow_all: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn auto_deny() -> Self {
        Self {
            confirmer: Arc::new(|_| Decision::Deny),
            yolo: false,
            allow_all: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_confirmer(c: Confirmer) -> Self {
        Self {
            confirmer: c,
            yolo: false,
            allow_all: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the gate currently lets every operation through without asking.
    /// True when constructed via [`PermissionGate::yolo`], or after any confirmer
    /// answered [`Decision::AllowAll`].
    pub fn is_permissive(&self) -> bool {
        self.yolo || self.allow_all.load(Ordering::SeqCst)
    }

    /// Toggle the session-wide "allow all" latch. This is the same latch
    /// [`Decision::AllowAll`] sets, so toggling on behaves exactly like answering
    /// `[a]ll` once. Toggling off restores per-call confirmation — unless this gate
    /// was built with [`PermissionGate::yolo`], in which case it stays permissive
    /// (that mode is a CLI flag, not a runtime toggle).
    pub fn set_allow_all(&self, on: bool) {
        self.allow_all.store(on, Ordering::SeqCst);
    }

    pub fn check(&self, p: Prompt) -> Result<(), ToolError> {
        if self.yolo || self.allow_all.load(Ordering::SeqCst) {
            return Ok(());
        }
        match (self.confirmer)(p.clone()) {
            Decision::Allow => Ok(()),
            Decision::AllowAll => {
                self.allow_all.store(true, Ordering::SeqCst);
                Ok(())
            }
            Decision::Deny => Err(ToolError::Exec(format!(
                "permission denied: {} ({})",
                p.summary, p.tool
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn prompt() -> Prompt {
        Prompt {
            tool: "bash",
            summary: "rm -rf /tmp/x".into(),
        }
    }

    #[test]
    fn yolo_and_auto_approve_allow() {
        assert!(PermissionGate::yolo().check(prompt()).is_ok());
        assert!(PermissionGate::auto_approve().check(prompt()).is_ok());
    }

    #[test]
    fn auto_deny_denies() {
        assert!(PermissionGate::auto_deny().check(prompt()).is_err());
    }

    #[test]
    fn confirmer_called_and_allow_all_latches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let gate = PermissionGate::with_confirmer(Arc::new(move |_p| {
            calls2.fetch_add(1, Ordering::SeqCst);
            Decision::AllowAll
        }));
        gate.check(prompt()).unwrap();
        gate.check(prompt()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn deny_returns_error_not_panic() {
        let gate = PermissionGate::with_confirmer(Arc::new(|_| Decision::Deny));
        let err = gate.check(prompt()).unwrap_err();
        assert!(matches!(err, crate::tool::ToolError::Exec(_)));
    }
}
