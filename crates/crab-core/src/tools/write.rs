use crate::gate::{PermissionGate, Prompt};
use crate::state::SessionState;
use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub struct WriteTool {
    state: SessionState,
    gate: PermissionGate,
}

impl WriteTool {
    pub fn new(state: SessionState, gate: PermissionGate) -> Self {
        Self { state, gate }
    }

    /// Atomic write: fill a temp file in the same dir, then rename. A crash can't
    /// leave a half-written file (DESIGN §5 all-or-nothing).
    fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, content.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("write")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Write a file (created if missing, overwritten if present). Overwriting requires the file was read this session and is unchanged since (freshness gate). Writes are atomic.",
        )
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'content'".into()))?;
        let p = PathBuf::from(path);

        if p.exists() && !self.state.is_fresh(&p) {
            return Err(ToolError::Exec(format!(
                "refusing to overwrite {path}: not read this session or changed externally. read it first"
            )));
        }
        self.gate.check(Prompt {
            tool: "write",
            summary: format!("write {path}"),
        })?;
        Self::atomic_write(&p, content)
            .map_err(|e| ToolError::Exec(format!("write {path}: {e}")))?;
        // Just wrote it = fresh (content is what we wrote).
        if let Ok(m) = std::fs::metadata(&p).and_then(|md| md.modified()) {
            self.state.mark_read(p, m);
        }
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::PermissionGate;
    use crate::state::SessionState;
    use crate::tool::Tool;

    fn setup() -> (tempfile::TempDir, SessionState, PermissionGate) {
        (
            tempfile::tempdir().unwrap(),
            SessionState::default(),
            PermissionGate::auto_approve(),
        )
    }

    #[tokio::test]
    async fn creates_new_file() {
        let (dir, state, gate) = setup();
        let p = dir.path().join("new.txt");
        let t = WriteTool::new(state, gate);
        t.execute(serde_json::json!({"path": p.to_str().unwrap(), "content": "abc"}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "abc");
    }

    #[tokio::test]
    async fn overwrite_requires_freshness() {
        let (dir, state, gate) = setup();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "old").unwrap();
        let t = WriteTool::new(state.clone(), gate.clone());
        let r = t
            .execute(serde_json::json!({"path": p.to_str().unwrap(), "content": "new"}))
            .await;
        assert!(r.is_err(), "overwrite without prior read must be rejected");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
    }

    #[tokio::test]
    async fn denied_by_gate_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.txt");
        std::fs::write(&p, "orig").unwrap();
        let state = SessionState::default();
        let reader = crate::tools::read::ReadTool::new(state.clone());
        reader
            .execute(serde_json::json!({"path": p.to_str().unwrap()}))
            .await
            .unwrap();
        let t = WriteTool::new(state, PermissionGate::auto_deny());
        let r = t
            .execute(serde_json::json!({"path": p.to_str().unwrap(), "content": "x"}))
            .await;
        assert!(r.is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "orig");
    }
}
