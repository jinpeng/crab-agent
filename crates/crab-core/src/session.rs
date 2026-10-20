use crate::types::{EntryId, Message};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 当前 writer 格式版本；唯一手工维护的事实源（DSH 式纪律）。
/// v1 文件无 meta 行；v2 起首行为 Meta。
pub const SESSION_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        id: EntryId,
        parent_id: Option<EntryId>,
        message: Message,
    },
    Meta {
        format_version: u32,
        created: String,
    },
    Obs {
        id: EntryId,
        event: crate::obs::ObsEvent,
    },
}

pub struct Session {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    next_id: EntryId,
    format_version: u32,
}

impl Session {
    /// Create a unique session file under dir (name = nanosecond timestamp).
    pub fn create(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!("session-{nanos}.jsonl"));
        let mut f = std::fs::File::create(&path)?;
        let created = format!("{}ms", crate::util::now_ms());
        let meta = SessionEntry::Meta {
            format_version: SESSION_FORMAT_VERSION,
            created,
        };
        writeln!(f, "{}", serde_json::to_string(&meta).map_err(std::io::Error::other)?)?;
        Ok(Self {
            path,
            entries: vec![meta],
            next_id: 1,
            format_version: SESSION_FORMAT_VERSION,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Number of entries in the session.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the session has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rebuild from a JSONL file (for --continue). Parses line by line; any bad
    /// line errors. A leading `meta` row declares the format version; files
    /// without one are v1 (pre-observability sessions).
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();
        let mut max_id = None::<EntryId>;
        let mut format_version = 1u32;
        for (n, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionEntry = serde_json::from_str(line).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}:{}: {}", path.display(), n + 1, e),
                )
            })?;
            if let SessionEntry::Meta { format_version: v, .. } = &entry {
                format_version = *v;
            }
            max_id = Some(max_id.map_or(entry.id(), |m: EntryId| m.max(entry.id())));
            entries.push(entry);
        }
        Ok(Self {
            path: path.to_path_buf(),
            next_id: max_id.map(|m| m + 1).unwrap_or(0),
            entries,
            format_version,
        })
    }

    /// The session file's format version (1 = no meta row, 2+ = declared).
    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Only the model-visible messages — obs/meta entries never enter the
    /// conversation history (观测不改变系统).
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.entries.iter().filter_map(|e| match e {
            SessionEntry::Message { message, .. } => Some(message),
            _ => None,
        })
    }

    pub fn next_id(&self) -> EntryId {
        self.next_id
    }

    pub fn append(&mut self, message: Message) -> std::io::Result<EntryId> {
        let id = self.next_id;
        // Chain messages among themselves: meta/obs entries never join the
        // parent chain, so parent semantics match v1 files.
        let parent_id = self
            .entries
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEntry::Message { id, .. } => Some(*id),
                _ => None,
            });
        let entry = SessionEntry::Message {
            id,
            parent_id,
            message,
        };
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&entry).map_err(std::io::Error::other)?
        )?;
        self.entries.push(entry);
        self.next_id += 1;
        Ok(id)
    }

    /// Append an observability event. Same file, same ordering; failures are
    /// the caller's problem (agent loop warns and continues).
    pub fn append_obs(&mut self, event: crate::obs::ObsEvent) -> std::io::Result<EntryId> {
        let id = self.next_id;
        let entry = SessionEntry::Obs { id, event };
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&entry).map_err(std::io::Error::other)?
        )?;
        self.entries.push(entry);
        self.next_id += 1;
        Ok(id)
    }
}

impl SessionEntry {
    pub fn id(&self) -> EntryId {
        match self {
            SessionEntry::Message { id, .. } | SessionEntry::Obs { id, .. } => *id,
            // Meta is written once at create time with id semantics unused; it
            // sits at the head and never joins the parent chain.
            SessionEntry::Meta { .. } => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, Role};

    fn msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn create_then_append_assigns_sequential_ids_and_parent() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        let a = s.append(msg("first")).unwrap();
        let b = s.append(msg("second")).unwrap();
        // Entry 0 is the v2 meta row, so messages start at id 1.
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert!(matches!(s.entries()[0], SessionEntry::Meta { .. }));
        match &s.entries()[1] {
            SessionEntry::Message { parent_id, .. } => assert_eq!(*parent_id, None),
            other => panic!("expected message, got {other:?}"),
        }
        match &s.entries()[2] {
            SessionEntry::Message { parent_id, .. } => assert_eq!(*parent_id, Some(1)),
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn file_is_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("x")).unwrap();
        s.append(msg("y")).unwrap();
        let raw = std::fs::read_to_string(s.path()).unwrap();
        assert_eq!(raw.lines().count(), 3); // meta row + two messages
        for line in raw.lines() {
            serde_json::from_str::<SessionEntry>(line).unwrap();
        }
    }

    #[test]
    fn load_reconstructs_entries_and_next_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("a")).unwrap();
        s.append(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "b".into() }],
        })
        .unwrap();
        let path = s.path().to_path_buf();
        drop(s);

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.entries().len(), 3); // meta + two messages
        assert_eq!(loaded.next_id(), 3);
        assert_eq!(loaded.format_version(), SESSION_FORMAT_VERSION);
    }

    #[test]
    fn load_continues_appending_after_existing() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("a")).unwrap();
        let path = s.path().to_path_buf();
        drop(s);

        let mut loaded = Session::load(&path).unwrap();
        let id = loaded.append(msg("c")).unwrap();
        assert_eq!(id, 2); // meta(0) + message(1) already exist
    }

    #[test]
    fn len_tracks_number_of_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        assert_eq!(s.len(), 1); // the v2 meta row
        s.append(msg("a")).unwrap();
        s.append(msg("b")).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.len(), s.entries().len());
    }

    #[test]
    fn v2_writes_meta_first_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("hi")).unwrap();
        s.append_obs(crate::obs::ObsEvent {
            run_id: 0,
            ts: 1,
            body: crate::obs::ObsBody::TurnStart,
        })
        .unwrap();
        let text = std::fs::read_to_string(s.path()).unwrap();
        let first: serde_json::Value =
            serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["kind"], "meta");
        assert_eq!(first["format_version"], 2);

        let back = Session::load(s.path()).unwrap();
        assert_eq!(back.format_version(), 2);
        assert_eq!(back.messages().count(), 1);
        assert!(matches!(&back.entries()[2], SessionEntry::Obs { .. }));
    }

    #[test]
    fn v1_file_without_meta_loads_as_version_1() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        std::fs::write(
            &p,
            "{\"kind\":\"message\",\"id\":0,\"parent_id\":null,\"message\":{\"role\":\"user\",\"content\":[]}}\n",
        )
        .unwrap();
        let s = Session::load(&p).unwrap();
        assert_eq!(s.format_version(), 1);
        assert_eq!(s.messages().count(), 1);
    }

    #[test]
    fn load_rejects_garbage_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.jsonl");
        std::fs::write(&p, "{\"kind\":\"message\",\"id\":0,\"parent_id\":null,\"message\":{\"role\":\"user\",\"content\":[]}}\nnot-json\n").unwrap();
        assert!(Session::load(&p).is_err());
    }
}
