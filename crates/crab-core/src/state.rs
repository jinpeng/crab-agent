use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Cross-tool session state: which files were read this session and at what mtime.
/// edit/write's freshness gate uses this to detect "model saw stale content" (Q14).
#[derive(Clone, Default)]
pub struct SessionState {
    inner: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
}

impl SessionState {
    /// Record that `path` was read at `mtime`.
    pub fn mark_read(&self, path: PathBuf, mtime: SystemTime) {
        self.inner.lock().unwrap().insert(path, mtime);
    }

    /// Fresh = read this session AND on-disk mtime still equals the recorded mtime.
    pub fn is_fresh(&self, path: &Path) -> bool {
        let recorded = match self.inner.lock().unwrap().get(path) {
            Some(m) => *m,
            None => return false,
        };
        match std::fs::metadata(path).and_then(|md| md.modified()) {
            Ok(current) => current == recorded,
            Err(_) => false, // file gone = not fresh
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_after_mark_read_then_stale_after_external_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "v1").unwrap();
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();

        let state = SessionState::default();
        assert!(!state.is_fresh(&p));
        state.mark_read(p.clone(), mtime);
        assert!(state.is_fresh(&p));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, "v2-changed-externally").unwrap();
        assert!(!state.is_fresh(&p));
    }

    #[test]
    fn shared_via_clone_sees_same_state() {
        let a = SessionState::default();
        let b = a.clone();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.txt");
        std::fs::write(&p, "hi").unwrap();
        let m = std::fs::metadata(&p).unwrap().modified().unwrap();
        a.mark_read(p.clone(), m);
        assert!(b.is_fresh(&p));
    }
}
