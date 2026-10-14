use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use std::borrow::Cow;
use std::path::Path;

pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self {
        Self
    }

    /// `root` stays the original search root across recursion so `rel` keeps its
    /// full relative prefix (nested/lib.rs, not lib.rs); `dir` is the current level.
    fn walk(
        root: &Path,
        dir: &Path,
        matcher: &GlobMatcher,
        out: &mut Vec<String>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if path.is_dir() {
                Self::walk(root, &path, matcher, out)?;
            } else if matcher.is_match(rel) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
        Ok(())
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("glob")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Find files by glob pattern (e.g. \"**/*.rs\"). Returns matching paths relative to root, one per line.",
        )
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob pattern" },
                "root": { "type": "string", "description": "directory to search (default: cwd)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'pattern'".into()))?;
        let root = args["root"]
            .as_str()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let matcher = Glob::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?
            .compile_matcher();
        let mut matches = Vec::new();
        Self::walk(&root, &root, &matcher, &mut matches)
            .map_err(|e| ToolError::Exec(format!("glob {pattern}: {e}")))?;
        matches.sort();
        if matches.is_empty() {
            Ok("no matches".into())
        } else {
            Ok(matches.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/nested")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "").unwrap();
        std::fs::write(d.path().join("src/nested/lib.rs"), "").unwrap();
        std::fs::write(d.path().join("README.md"), "").unwrap();
        d
    }

    #[tokio::test]
    async fn matches_rs_recursively() {
        let d = tree();
        let t = GlobTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "**/*.rs", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(out.contains("main.rs"));
        assert!(out.contains("nested/lib.rs"));
        assert!(!out.contains("README.md"));
    }

    #[tokio::test]
    async fn no_matches_returns_empty_not_error() {
        let d = tree();
        let t = GlobTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "*.xyz", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(out.contains("no matches") || out.trim().is_empty());
    }
}
