use crate::types::{EntryId, Message};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        id: EntryId,
        parent_id: Option<EntryId>,
        message: Message,
    },
}

pub struct Session {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    next_id: EntryId,
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
        std::fs::File::create(&path)?;
        Ok(Self {
            path,
            entries: Vec::new(),
            next_id: 0,
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

    /// Rebuild from a JSONL file (for --continue). Parses line by line; any bad line errors.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut entries = Vec::new();
        let mut max_id = None::<EntryId>;
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
            max_id = Some(max_id.map_or(entry.id(), |m: EntryId| m.max(entry.id())));
            entries.push(entry);
        }
        Ok(Self {
            path: path.to_path_buf(),
            next_id: max_id.map(|m| m + 1).unwrap_or(0),
            entries,
        })
    }

    pub fn next_id(&self) -> EntryId {
        self.next_id
    }

    pub fn append(&mut self, message: Message) -> std::io::Result<EntryId> {
        let id = self.next_id;
        let parent_id = self.entries.last().map(|e| e.id());
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
}

impl SessionEntry {
    pub fn id(&self) -> EntryId {
        match self {
            SessionEntry::Message { id, .. } => *id,
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
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        match &s.entries()[1] {
            SessionEntry::Message { parent_id, .. } => assert_eq!(*parent_id, Some(0)),
        }
    }

    #[test]
    fn file_is_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        s.append(msg("x")).unwrap();
        s.append(msg("y")).unwrap();
        let raw = std::fs::read_to_string(s.path()).unwrap();
        assert_eq!(raw.lines().count(), 2);
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
        assert_eq!(loaded.entries().len(), 2);
        assert_eq!(loaded.next_id(), 2);
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
        assert_eq!(id, 1);
    }

    #[test]
    fn len_tracks_number_of_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(dir.path()).unwrap();
        assert_eq!(s.len(), 0);
        s.append(msg("a")).unwrap();
        s.append(msg("b")).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.len(), s.entries().len());
    }

    #[test]
    fn load_rejects_garbage_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.jsonl");
        std::fs::write(&p, "{\"kind\":\"message\",\"id\":0,\"parent_id\":null,\"message\":{\"role\":\"user\",\"content\":[]}}\nnot-json\n").unwrap();
        assert!(Session::load(&p).is_err());
    }
}
