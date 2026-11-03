//! Lesson library: the derived view over [`ObsBody::Lesson`] events
//! (design: docs/notes/2026-11-03-crab-memory-minimal-design.md).
//!
//! 日志即真相: lessons live only as obs rows in session JSONL files;
//! this module replays them into a live set. Same pattern as
//! [`crate::compact::resolve`]. A correction never edits the log — it
//! appends a new Lesson event whose `supersedes` retires the old entry
//! (same-session EntryIds only in v1).

use crate::obs::{LessonKind, ObsBody};
use crate::session::{Session, SessionEntry};
use crate::types::EntryId;
use std::path::{Path, PathBuf};

/// 一条一课的长度上限（/reflect 提议与注入共用，design §数据模型）。
pub const MAX_LESSON_TEXT_CHARS: usize = 200;
/// Lessons 注入段的 always-on 预算。tokens≈chars/4 估算（与 meter 的
/// 字节折算同级的近似；design §注入定为 1500 tokens）。
pub const LESSON_SECTION_BUDGET_CHARS: usize = 1500 * 4;

/// A lesson that survives replay (not superseded by a later event).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveLesson {
    /// Which session file produced this lesson (ids are per-session).
    pub session: PathBuf,
    /// The Lesson obs entry's id within `session`.
    pub id: EntryId,
    pub kind: LessonKind,
    pub text: String,
    /// EntryId of the triggering entry, same session as `id`.
    pub evidence: EntryId,
}

/// Replay the Lesson events of one session's entries → surviving lessons
/// in log order (oldest first). Dangling `supersedes` (target id never
/// seen) is ignored, mirroring compact::resolve's defensive stance.
pub fn resolve(entries: &[SessionEntry], session: PathBuf) -> Vec<LiveLesson> {
    let mut live: Vec<LiveLesson> = Vec::new();
    for e in entries {
        if let SessionEntry::Obs {
            id,
            event:
                crate::obs::ObsEvent {
                    body:
                        ObsBody::Lesson {
                            kind,
                            text,
                            evidence,
                            supersedes,
                        },
                    ..
                },
        } = e
        {
            if let Some(old) = supersedes {
                live.retain(|l| l.id != *old);
            }
            live.push(LiveLesson {
                session: session.clone(),
                id: *id,
                kind: *kind,
                text: text.clone(),
                evidence: *evidence,
            });
        }
    }
    live
}

/// The per-project library: replay every `*.jsonl` session under `dir`
/// (typically `.crab/sessions`), merge in filename order. Returns log
/// order overall (session file order, then entry order within a file) —
/// callers sort for injection (most recent first). Missing dir = empty
/// library. Cross-session supersede is not supported in v1.
pub fn load_library(dir: &Path) -> std::io::Result<Vec<LiveLesson>> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let session = Session::load(&f)?;
        out.extend(resolve(session.entries(), f));
    }
    Ok(out)
}

/// Parse an LLM's /reflect reply into lesson proposals. Strict line format
/// `FACT|text` / `PROCEDURE|text`（大小写不敏感）；超长、空文本、重复项
/// 丢弃。解析失败返回空列表——为凑数硬造 lesson 比空手而归更糟
/// （KEEP_BASE 纪律）。
pub fn parse_proposals(text: &str) -> Vec<(LessonKind, String)> {
    let mut out: Vec<(LessonKind, String)> = Vec::new();
    for line in text.lines() {
        let Some((kind, body)) = line.split_once('|') else {
            continue;
        };
        let kind = match kind.trim().to_ascii_uppercase().as_str() {
            "FACT" => LessonKind::Fact,
            "PROCEDURE" => LessonKind::Procedure,
            _ => continue,
        };
        let text = body.trim();
        if text.is_empty() || text.chars().count() > MAX_LESSON_TEXT_CHARS {
            continue;
        }
        if out.iter().any(|(_, t)| t == text) {
            continue;
        }
        out.push((kind, text.to_string()));
    }
    out
}

/// Render the system-prompt Lessons section. `lessons` must be sorted
/// newest-first（调用方负责，load_library 产出为日志序）；超预算截断
/// 尾部并提示剩余条数，而不是静默丢弃或撑爆上下文（P0-1 纪律）。
/// 空列表 → None（无该段）。
pub fn render_section(lessons_newest_first: &[LiveLesson]) -> Option<String> {
    if lessons_newest_first.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n\n# Lessons (from previous sessions in this project)\n\
         Each line is a distilled, human-confirmed lesson from a past session.\n",
    );
    let mut omitted = 0usize;
    for l in lessons_newest_first {
        let line = format!("- [{}] {}\n", l.kind, l.text);
        if out.len() + line.len() > LESSON_SECTION_BUDGET_CHARS && out.lines().count() > 3 {
            // 已至少有标题+一条 → 截断；第一条永远放行（再小也留个种子）。
            omitted += 1;
            continue;
        }
        out.push_str(&line);
    }
    if omitted > 0 {
        out.push_str(&format!(
            "- … {omitted} more lessons on disk (.crab/sessions) — run /reflect in a past session to revisit\n"
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::ObsEvent;

    fn lesson(id: EntryId, text: &str, supersedes: Option<EntryId>) -> SessionEntry {
        SessionEntry::Obs {
            id,
            event: ObsEvent {
                run_id: 0,
                ts: id,
                body: ObsBody::Lesson {
                    kind: LessonKind::Fact,
                    text: text.into(),
                    evidence: id - 1,
                    supersedes,
                },
            },
        }
    }

    #[test]
    fn parse_proposals_keeps_valid_lines_only() {
        let text = "PROCEDURE|run cargo insta review after snapshot tests\nfact|sessions live under .crab/sessions\n\nNOT_A_KIND|skip me\nPROCEDURE|\nPROCEDURE|duplicate\nPROCEDURE|duplicate\n";
        let got = parse_proposals(text);
        assert_eq!(
            got,
            vec![
                (
                    LessonKind::Procedure,
                    "run cargo insta review after snapshot tests".to_string()
                ),
                (
                    LessonKind::Fact,
                    "sessions live under .crab/sessions".to_string()
                ),
                (LessonKind::Procedure, "duplicate".to_string()),
            ]
        );
    }

    #[test]
    fn parse_proposals_drops_overlong_lessons() {
        let long = "x".repeat(MAX_LESSON_TEXT_CHARS + 1);
        let got = parse_proposals(&format!("FACT|{long}\nFACT|ok"));
        assert_eq!(got, vec![(LessonKind::Fact, "ok".to_string())]);
    }

    fn live(text: &str) -> LiveLesson {
        LiveLesson {
            session: PathBuf::from("s.jsonl"),
            id: 1,
            kind: LessonKind::Fact,
            text: text.into(),
            evidence: 0,
        }
    }

    #[test]
    fn render_section_formats_newest_first() {
        let lessons = vec![live("newest"), live("oldest")];
        let section = render_section(&lessons).unwrap();
        assert!(section.contains("# Lessons (from previous sessions in this project)"));
        assert!(section.contains("- [fact] newest\n- [fact] oldest"));
    }

    #[test]
    fn render_section_none_when_empty() {
        assert_eq!(render_section(&[]), None);
    }

    #[test]
    fn render_section_truncates_with_omitted_hint() {
        // 每条 ~150 字符 × 50 条 ≈ 7.5k > 6k 预算，确保截断真的发生。
        let lessons: Vec<_> = (0..50)
            .map(|i| live(&format!("lesson-{i:02} {}", "pad".repeat(48))))
            .collect();
        let section = render_section(&lessons).unwrap();
        // 预算内只出现头部若干条，尾部有剩余提示。
        assert!(section.contains("lesson-00"));
        assert!(!section.contains("lesson-49"));
        assert!(section.contains("more lessons on disk"));
    }

    #[test]
    fn resolve_collects_lessons_in_log_order() {
        let entries = vec![lesson(2, "a", None), lesson(3, "b", None)];
        let live = resolve(&entries, PathBuf::from("s.jsonl"));
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].text, "a");
        assert_eq!(live[1].text, "b");
        assert_eq!(live[0].session, PathBuf::from("s.jsonl"));
        assert_eq!(live[1].evidence, 2);
    }

    #[test]
    fn supersede_retires_earlier_lesson() {
        let entries = vec![lesson(2, "old", None), lesson(5, "new", Some(2))];
        let live = resolve(&entries, PathBuf::from("s.jsonl"));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].text, "new");
        assert_eq!(live[0].id, 5);
    }

    #[test]
    fn dangling_supersedes_is_ignored() {
        let entries = vec![lesson(2, "only", Some(99))];
        let live = resolve(&entries, PathBuf::from("s.jsonl"));
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn non_lesson_entries_are_skipped() {
        let entries = vec![
            SessionEntry::Obs {
                id: 1,
                event: ObsEvent {
                    run_id: 0,
                    ts: 1,
                    body: ObsBody::TurnStart,
                },
            },
            lesson(2, "x", None),
        ];
        let live = resolve(&entries, PathBuf::from("s.jsonl"));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, 2);
    }

    #[test]
    fn load_library_merges_sessions_in_filename_order() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [("session-1.jsonl", "first"), ("session-2.jsonl", "second")] {
            let line = serde_json::json!({
                "kind": "obs",
                "id": 1,
                "event": {"run_id": 0, "ts": 1, "obs": "lesson",
                          "kind": "fact", "text": text, "evidence": 0, "supersedes": null}
            });
            std::fs::write(dir.path().join(name), format!("{line}\n")).unwrap();
        }
        let live = load_library(dir.path()).unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].text, "first");
        assert_eq!(live[1].text, "second");
        assert!(live[0].session.ends_with("session-1.jsonl"));
    }

    #[test]
    fn load_library_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let live = load_library(&dir.path().join("nope")).unwrap();
        assert!(live.is_empty());
    }
}
