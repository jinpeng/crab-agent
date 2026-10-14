use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use grep_regex::RegexMatcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use std::borrow::Cow;
use std::path::Path;

const MAX_MATCHES: usize = 200;

pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }

    /// Collect candidate files under `root` using ripgrep's traversal rules:
    /// `.gitignore`/`.ignore` are respected and hidden entries (like `.git/`)
    /// are skipped.
    fn walk(root: &Path) -> Vec<std::path::PathBuf> {
        WalkBuilder::new(root)
            // Honor `.gitignore` even when the tree is not a git checkout.
            .require_git(false)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
            .map(|entry| entry.into_path())
            .collect()
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("grep")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Search file contents by regex. Returns `path:line:content` per match (max 200). \
         Respects .gitignore/.ignore and skips hidden files and directories.",
        )
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

        // A missing root keeps erroring like before (the walker itself would
        // just yield no entries).
        std::fs::metadata(&root)
            .map_err(|e| ToolError::Exec(format!("{}: {e}", root.display())))?;

        let mut files = Self::walk(&root);
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

    #[tokio::test]
    async fn finds_match_in_normal_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("src.rs"), "let needle = 1;\n").unwrap();
        let t = GrepTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "needle", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();
        assert!(out.contains("src.rs:1:let needle = 1;"), "got {out:?}");
    }

    #[tokio::test]
    async fn respects_gitignore_and_skips_hidden_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::create_dir_all(d.path().join(".hidden")).unwrap();
        std::fs::write(d.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::write(d.path().join("keep.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(d.path().join("target/gen.rs"), "let needle = 2;\n").unwrap();
        std::fs::write(d.path().join(".hidden/h.rs"), "let needle = 3;\n").unwrap();

        let t = GrepTool::new();
        let out = t
            .execute(serde_json::json!({
                "pattern": "needle", "root": d.path().to_str().unwrap()
            }))
            .await
            .unwrap();

        assert!(out.contains("keep.rs:1:let needle = 1;"), "got {out:?}");
        assert!(!out.contains("target/"), "gitignored dir leaked: {out:?}");
        assert!(!out.contains(".hidden/"), "hidden dir leaked: {out:?}");
    }
}
