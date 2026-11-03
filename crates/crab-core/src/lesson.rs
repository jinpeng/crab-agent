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
