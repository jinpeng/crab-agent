use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use grep_regex::RegexMatcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use std::path::Path;

const MAX_MATCHES: usize = 200;

pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }

    fn walk(root: &Path, files: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::walk(&path, files)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents by regex. Returns `path:line:content` per match (max 200)."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "regex" },
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
        let matcher = RegexMatcher::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad regex: {e}")))?;

        let mut files = Vec::new();
        Self::walk(&root, &mut files).map_err(|e| ToolError::Exec(e.to_string()))?;
        files.sort();

        let mut results: Vec<String> = Vec::new();
        'files: for file in files {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            let mut searcher = SearcherBuilder::new().build();
            let mut per_file = Vec::new();
            let current = results.len();
            let _ = searcher.search_path(
                &matcher,
                &file,
                UTF8(|line_no, line| {
                    per_file.push(format!("{rel}:{line_no}:{}", line.trim_end()));
                    Ok(current + per_file.len() < MAX_MATCHES) // stop at cap
                }),
            );
            results.extend(per_file);
            if results.len() >= MAX_MATCHES {
                break 'files;
            }
        }

        if results.is_empty() {
            Ok("no matches".into())
        } else {
            let mut out = results.join("\n");
            if results.len() >= MAX_MATCHES {
                out.push_str(&format!("\n... [truncated at {MAX_MATCHES} matches]"));
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "nothing here\n").unwrap();
        d
    }

    #[tokio::test]
    async fn finds_pattern_with_line_numbers() {
        let d = tree();
        let t = GrepTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "fn \\w+", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(out.contains("a.rs:1:"));
        assert!(out.contains("a.rs:2:"));
        assert!(!out.contains("b.txt"));
    }

    #[tokio::test]
    async fn invalid_regex_is_error_not_panic() {
        let d = tree();
        let t = GrepTool::new();
        let r = t
            .execute(serde_json::json!({
                "pattern": "([invalid", "root": d.path().to_str().unwrap()
            }))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn no_matches_returns_clean_message() {
        let d = tree();
        let t = GrepTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "zzz_not_present", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(out.contains("no matches") || out.trim().is_empty());
    }
}
