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
}
