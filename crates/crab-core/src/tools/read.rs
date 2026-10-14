use crate::state::SessionState;
use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use std::borrow::Cow;
use std::path::PathBuf;

const MAX_BYTES: usize = 100_000;

pub struct ReadTool {
    state: SessionState,
}

impl ReadTool {
    pub fn new(state: SessionState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("read")
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file. Returns contents (truncated past 100KB with a notice). Records the file as read for the freshness gate, so edit/write can detect external changes."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "absolute or cwd-relative path" } },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path' string".into()))?;
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| ToolError::Exec(format!("read {path}: {e}")))?;
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| ToolError::Exec(format!("stat {path}: {e}")))?;
        self.state.mark_read(PathBuf::from(path), mtime);

        let text = String::from_utf8_lossy(&bytes);
        if text.len() > MAX_BYTES {
            // Step the cut back to a char boundary: byte-slicing a multi-byte char panics.
            let mut cut = MAX_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let keep = &text[..cut];
            Ok(format!(
                "{keep}\n... [truncated {} bytes]",
                text.len() - cut
            ))
        } else {
            Ok(text.into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use crate::tool::Tool;

    #[tokio::test]
    async fn reads_file_and_marks_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello world").unwrap();
        let state = SessionState::default();
        let t = ReadTool::new(state.clone());
        let out = t
            .execute(serde_json::json!({"path": p.to_str().unwrap()}))
            .await
            .unwrap();
        assert_eq!(out, "hello world");
        assert!(state.is_fresh(&p));
    }

    #[tokio::test]
    async fn missing_file_is_error_not_panic() {
        let t = ReadTool::new(SessionState::default());
        let r = t
            .execute(serde_json::json!({"path": "/nonexistent/xyz.txt"}))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn large_file_truncated_with_notice() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.txt");
        std::fs::write(&p, "x".repeat(200_000)).unwrap();
        let t = ReadTool::new(SessionState::default());
        let out = t
            .execute(serde_json::json!({"path": p.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(out.len() < 200_000);
        assert!(out.contains("truncated"));
    }
}

#[cfg(test)]
mod utf8_boundary_tests {
    use super::*;
    use crate::state::SessionState;
    use crate::tool::Tool;

    #[tokio::test]
    async fn truncation_does_not_panic_on_multibyte_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mb.txt");
        // '中' is 3 bytes; pad so the 100KB cut lands inside a multi-byte char.
        let mut content = "x".repeat(99_999);
        content.push_str("中文中文中文");
        content.push_str(&"y".repeat(200_000));
        std::fs::write(&p, content).unwrap();
        let t = ReadTool::new(SessionState::default());
        let out = t
            .execute(serde_json::json!({"path": p.to_str().unwrap()}))
            .await
            .unwrap();
        assert!(out.contains("truncated"));
    }
}
