#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Edit {
    pub old: String,
    pub new: String,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EditError {
    #[error("no match for old text. closest: {0}")]
    NoMatch(String),
    #[error("old text matches {count} times (must be unique). first at byte {first_byte}")]
    NotUnique { count: usize, first_byte: usize },
    #[error("edits overlap")]
    Overlap,
    #[error("old text must be non-empty")]
    EmptyOld,
}

/// Apply multiple edits to content. Each `old` must match the ORIGINAL text exactly
/// once (literal, no whitespace tolerance) and edits must not overlap. All-or-nothing:
/// any failure yields Err and no partial result (Q14).
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, EditError> {
    // 1. Find all match positions per edit; require uniqueness.
    let mut spans: Vec<(usize, usize, &str)> = Vec::new(); // (start, end, new)
    for e in edits {
        if e.old.is_empty() {
            // Empty `old` is meaningless (matches everywhere / once on empty content);
            // reject explicitly rather than produce a garbage no-op edit.
            return Err(EditError::EmptyOld);
        }
        let matches: Vec<usize> = content.match_indices(&e.old).map(|(i, _)| i).collect();
        match matches.len() {
            0 => return Err(EditError::NoMatch(closest_hint(content, &e.old))),
            1 => spans.push((matches[0], matches[0] + e.old.len(), &e.new)),
            n => {
                return Err(EditError::NotUnique {
                    count: n,
                    first_byte: matches[0],
                });
            }
        }
    }
    // 2. Sort by start, reject overlaps.
    spans.sort_by_key(|(s, _, _)| *s);
    for w in spans.windows(2) {
        if w[0].1 > w[1].0 {
            return Err(EditError::Overlap);
        }
    }
    // 3. Replace back-to-front so earlier byte offsets stay valid.
    let mut out = content.to_string();
    for (start, end, new) in spans.iter().rev() {
        out.replace_range(*start..*end, new);
    }
    Ok(out)
}

/// `similar` is used ONLY to build an error hint, never to decide where to edit (Q14).
fn closest_hint(content: &str, needle: &str) -> String {
    let mut best = (0.0f32, "");
    for line in content.lines() {
        let r = similar::TextDiff::from_chars(needle, line).ratio();
        if r > best.0 {
            best = (r, line);
        }
    }
    if best.0 > 0.3 {
        format!(
            "nearest line (similarity {:.0}%): `{}`",
            best.0 * 100.0,
            best.1.trim()
        )
    } else {
        "no similar line found".into()
    }
}

use crate::gate::{PermissionGate, Prompt};
use crate::state::SessionState;
use crate::tool::{Tool, ToolError};
use async_trait::async_trait;
use std::borrow::Cow;
use std::path::PathBuf;

pub struct EditTool {
    state: SessionState,
    gate: PermissionGate,
}

impl EditTool {
    pub fn new(state: SessionState, gate: PermissionGate) -> Self {
        Self { state, gate }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Apply exact-match edits to a file. Each edit's `old` must match the file EXACTLY once (literal, no whitespace tolerance) and edits must not overlap; all edits match against the original content and apply atomically (all-or-nothing). The file must have been read this session and be unchanged since.",
        )
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": { "type": "string", "description": "exact text, must match uniquely" },
                            "new": { "type": "string", "description": "replacement" }
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("missing 'path'".into()))?;
        let p = PathBuf::from(path);
        let edits: Vec<Edit> = serde_json::from_value(args["edits"].clone())
            .map_err(|e| ToolError::InvalidArgs(format!("bad 'edits': {e}")))?;
        if edits.is_empty() {
            return Err(ToolError::InvalidArgs("'edits' must be non-empty".into()));
        }

        if !self.state.is_fresh(&p) {
            return Err(ToolError::Exec(format!(
                "refusing to edit {path}: not read this session or changed externally. read it first"
            )));
        }
        let original = tokio::fs::read_to_string(&p)
            .await
            .map_err(|e| ToolError::Exec(format!("read {path}: {e}")))?;
        let updated = apply_edits(&original, &edits).map_err(|e| ToolError::Exec(e.to_string()))?;

        self.gate.check(Prompt {
            tool: "edit",
            summary: format!("edit {path} ({} edit(s))", edits.len()),
        })?;

        // Atomic write: same-dir temp file then rename.
        let dir = p.parent().unwrap_or_else(|| std::path::Path::new("."));
        let mut tmp =
            tempfile::NamedTempFile::new_in(dir).map_err(|e| ToolError::Exec(e.to_string()))?;
        std::io::Write::write_all(&mut tmp, updated.as_bytes())
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        tmp.persist(&p)
            .map_err(|e| ToolError::Exec(e.error.to_string()))?;

        if let Ok(m) = std::fs::metadata(&p).and_then(|md| md.modified()) {
            self.state.mark_read(p, m);
        }
        Ok(format!("applied {} edit(s) to {path}", edits.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(pairs: &[(&str, &str)]) -> Vec<Edit> {
        pairs
            .iter()
            .map(|(o, n)| Edit {
                old: o.to_string(),
                new: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn single_unique_match_applies() {
        let r = apply_edits("hello world", &edits(&[("world", "rust")])).unwrap();
        assert_eq!(r, "hello rust");
    }

    #[test]
    fn zero_matches_is_error() {
        let r = apply_edits("hello", &edits(&[("zzz", "x")]));
        assert!(matches!(r, Err(EditError::NoMatch(_))));
    }

    #[test]
    fn non_unique_match_is_error() {
        let r = apply_edits("foo foo foo", &edits(&[("foo", "bar")]));
        assert!(matches!(r, Err(EditError::NotUnique { count: 3, .. })));
    }

    #[test]
    fn multiple_edits_match_against_original() {
        let r = apply_edits("a=1\nb=2\nc=3", &edits(&[("a=1", "a=10"), ("c=3", "c=30")])).unwrap();
        assert_eq!(r, "a=10\nb=2\nc=30");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let r = apply_edits("abcdef", &edits(&[("abc", "X"), ("bcd", "Y")]));
        assert!(matches!(r, Err(EditError::Overlap)));
    }

    #[test]
    fn crlf_preserved_exactly() {
        let r = apply_edits("line1\r\nline2\r\n", &edits(&[("line2", "L2")])).unwrap();
        assert_eq!(r, "line1\r\nL2\r\n");
    }

    #[test]
    fn no_trailing_newline_preserved() {
        let r = apply_edits("no newline at end", &edits(&[("end", "END")])).unwrap();
        assert_eq!(r, "no newline at END");
    }

    #[test]
    fn utf8_multibyte_boundaries() {
        let r = apply_edits("你好，世界。再见", &edits(&[("世界", "Rust")])).unwrap();
        assert_eq!(r, "你好，Rust。再见");
    }

    #[test]
    fn error_includes_closest_candidate_hint() {
        let content = "fn main() {\n    println!(\"hi\");\n}\n";
        let r = apply_edits(
            content,
            &edits(&[("fn main() {\n    println!(\"hello\");\n}", "x")]),
        );
        match r {
            Err(EditError::NoMatch(hint)) => assert!(
                hint.contains("println"),
                "hint should point near closest match"
            ),
            other => panic!("expected NoMatch with hint, got {other:?}"),
        }
    }

    #[test]
    fn adjacent_edits_do_not_overlap() {
        // Edges touch but don't overlap: each replaces its own bytes.
        let r = apply_edits("abcdef", &edits(&[("ab", "X"), ("cd", "Y")])).unwrap();
        assert_eq!(r, "XYef");
    }

    #[test]
    fn empty_old_is_rejected() {
        // Empty `old` is meaningless; reject explicitly in all cases.
        let r = apply_edits("content", &edits(&[("", "x")]));
        assert!(matches!(r, Err(EditError::EmptyOld)));
        let r2 = apply_edits("", &edits(&[("", "x")]));
        assert!(matches!(r2, Err(EditError::EmptyOld)));
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use crate::gate::PermissionGate;
    use crate::state::SessionState;
    use crate::tool::Tool;
    use crate::tools::read::ReadTool;

    async fn fresh_file(
        dir: &tempfile::TempDir,
        name: &str,
        content: &str,
        state: &SessionState,
    ) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        ReadTool::new(state.clone())
            .execute(serde_json::json!({"path": p.to_str().unwrap()}))
            .await
            .unwrap();
        p
    }

    #[tokio::test]
    async fn edit_applies_to_disk_when_fresh_and_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let state = SessionState::default();
        let p = fresh_file(&dir, "a.txt", "hello world", &state).await;
        let t = EditTool::new(state, PermissionGate::auto_approve());
        t.execute(serde_json::json!({
            "path": p.to_str().unwrap(),
            "edits": [{"old": "world", "new": "rust"}]
        }))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello rust");
    }

    #[tokio::test]
    async fn edit_rejected_when_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let state = SessionState::default();
        let p = dir.path().join("b.txt");
        std::fs::write(&p, "data").unwrap();
        let t = EditTool::new(state, PermissionGate::auto_approve());
        let r = t
            .execute(serde_json::json!({
                "path": p.to_str().unwrap(), "edits": [{"old": "data", "new": "x"}]
            }))
            .await;
        assert!(r.is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "data");
    }

    #[tokio::test]
    async fn failed_match_leaves_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let state = SessionState::default();
        let original = "line1\r\nline2\r\n";
        let p = fresh_file(&dir, "c.txt", original, &state).await;
        let t = EditTool::new(state, PermissionGate::auto_approve());
        let r = t
            .execute(serde_json::json!({
                "path": p.to_str().unwrap(), "edits": [{"old": "nomatch", "new": "x"}]
            }))
            .await;
        assert!(r.is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }

    #[tokio::test]
    async fn edit_denied_by_gate_leaves_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = SessionState::default();
        let p = fresh_file(&dir, "d.txt", "keep me", &state).await;
        let t = EditTool::new(state, PermissionGate::auto_deny());
        let r = t
            .execute(serde_json::json!({
                "path": p.to_str().unwrap(), "edits": [{"old": "keep", "new": "x"}]
            }))
            .await;
        assert!(r.is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "keep me");
    }
}
