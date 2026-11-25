//! What a session's agent changed, turn by turn, so its files can go back to
//! how they were before any of the user's messages (or before the session):
//! what each file was before the agent's first write to it in each turn.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A file before an agent wrote it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Before {
    /// It didn't exist.
    Missing,
    /// Its open document's text (unsaved edits included), put back through
    /// the document.
    Text(String),
    /// Its bytes on disk (no open document), put back as they were, in
    /// whatever encoding.
    Bytes(#[serde(with = "base64_bytes")] Arc<[u8]>),
}

/// Bytes in a saved session as base64 text.
mod base64_bytes {
    use base64::Engine as _;
    use std::sync::Arc;

    pub fn serialize<S: serde::Serializer>(bytes: &Arc<[u8]>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Arc<[u8]>, D::Error> {
        let text: String = serde::Deserialize::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD.decode(text).map(Into::into).map_err(serde::de::Error::custom)
    }
}

impl Before {
    /// Its text as the editor would show it; None when it didn't exist.
    pub fn text(&self) -> Option<String> {
        match self {
            Before::Missing => None,
            Before::Text(text) => Some(text.clone()),
            Before::Bytes(bytes) => Some(crate::encoding::decode(bytes, None).0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Write {
    pub turn: usize,
    pub path: PathBuf,
    pub before: Before,
}

/// Kept with the session (`history::Saved`), so its files can still go
/// back after Kuana restarts.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Checkpoints {
    writes: Vec<Write>,
}

impl Checkpoints {
    /// Notes a write in `turn`; only the first one per file and turn counts.
    pub fn note(&mut self, turn: usize, path: &Path, before: Before) {
        if !self.writes.iter().any(|w| w.turn == turn && w.path == path) {
            self.writes.push(Write { turn, path: path.to_path_buf(), before });
        }
    }

    /// Whether `path`'s state before `turn` is kept.
    pub fn has(&self, turn: usize, path: &Path) -> bool {
        self.writes.iter().any(|w| w.turn == turn && w.path == path)
    }

    /// Whether anything was written in `turn` or after.
    pub fn changed_since(&self, turn: usize) -> bool {
        self.writes.iter().any(|w| w.turn >= turn)
    }

    /// The files to put back for the state before `turn`: each file written
    /// in it or later, with its text before the first of those writes.
    pub fn restore_to(&self, turn: usize) -> Vec<(PathBuf, Before)> {
        let mut files: Vec<(PathBuf, Before)> = Vec::new();
        let mut later: Vec<&Write> = self.writes.iter().filter(|w| w.turn >= turn).collect();
        later.sort_by_key(|w| w.turn);
        for write in later {
            if !files.iter().any(|(path, _)| *path == write.path) {
                files.push((write.path.clone(), write.before.clone()));
            }
        }
        files
    }

    /// Every file the session changed, with what it was before the session.
    pub fn files(&self) -> Vec<(PathBuf, Before)> {
        self.restore_to(0)
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// The files noted in `turn`, with what they were before it.
    pub fn of_turn(&self, turn: usize) -> Vec<(PathBuf, Before)> {
        self.writes.iter().filter(|w| w.turn == turn).map(|w| (w.path.clone(), w.before.clone())).collect()
    }

    /// Forgets `path`'s note in `turn` (it ended the turn as it began it: a
    /// change refused, or undone).
    pub fn forget(&mut self, turn: usize, path: &Path) {
        self.writes.retain(|w| !(w.turn == turn && w.path == path));
    }

    /// Forgets the writes of `turn` and after (they were undone).
    pub fn forget_from(&mut self, turn: usize) {
        self.writes.retain(|w| w.turn < turn);
    }

    /// The turns from `turn` on are forgotten but their files stay as the
    /// agent left them: their writes count as the turn before's (where that
    /// turn wrote a file too, its own, earlier one stays).
    pub fn fold_from(&mut self, turn: usize) {
        let into = turn.saturating_sub(1);
        let (mut kept, mut later): (Vec<Write>, Vec<Write>) = std::mem::take(&mut self.writes).into_iter().partition(|w| w.turn < turn);
        later.sort_by_key(|w| w.turn);
        for write in later {
            if !kept.iter().any(|k| k.turn == into && k.path == write.path) {
                kept.push(Write { turn: into, ..write });
            }
        }
        self.writes = kept;
    }

    pub fn clear(&mut self) {
        self.writes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_back_to_before_a_turn() {
        let mut c = Checkpoints::default();
        let (a, b) = (Path::new("/p/a.rs"), Path::new("/p/b.rs"));
        let text = |t: &str| Before::Text(t.into());
        c.note(1, a, text("a0"));
        c.note(1, a, text("a1")); // a second write in turn 1: still a0
        c.note(2, a, text("a2"));
        c.note(2, b, Before::Missing); // created in turn 2
        c.note(3, a, text("a3"));
        assert_eq!(c.restore_to(1), vec![(a.to_path_buf(), text("a0")), (b.to_path_buf(), Before::Missing)]);
        assert_eq!(c.files(), c.restore_to(1));
        assert_eq!(c.restore_to(2), vec![(a.to_path_buf(), text("a2")), (b.to_path_buf(), Before::Missing)]);
        assert_eq!(c.restore_to(3), vec![(a.to_path_buf(), text("a3"))]);
        assert!(c.restore_to(4).is_empty() && !c.changed_since(4));
        // Saved with the session and read back the same.
        let mut bytes = Checkpoints::default();
        bytes.note(1, a, Before::Bytes(vec![0, 255, 10].into()));
        let json = serde_json::to_string(&bytes).unwrap();
        assert_eq!(serde_json::from_str::<Checkpoints>(&json).unwrap().files(), bytes.files());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Checkpoints>(&json).unwrap().files(), c.files());
        assert_eq!(c.of_turn(2), vec![(a.to_path_buf(), text("a2")), (b.to_path_buf(), Before::Missing)]);
        c.forget(2, b);
        assert_eq!(c.of_turn(2), vec![(a.to_path_buf(), text("a2"))]);
        c.forget_from(2);
        assert_eq!(c.restore_to(1), vec![(a.to_path_buf(), text("a0"))]);
        c.clear();
        assert!(!c.changed_since(0) && c.is_empty());
        // Turns forgotten with their files kept: their writes join the turn
        // before, which keeps its own earlier one.
        let mut c = Checkpoints::default();
        c.note(1, a, text("a0"));
        c.note(2, a, text("a1"));
        c.note(2, b, Before::Missing);
        c.note(3, b, text("b1"));
        c.fold_from(2);
        assert_eq!(c.restore_to(1), vec![(a.to_path_buf(), text("a0")), (b.to_path_buf(), Before::Missing)]);
        assert!(!c.changed_since(2));
        // Bytes read back as the editor would decode them.
        assert_eq!(Before::Bytes(Arc::from(&b"caf\xe9"[..])).text().as_deref(), Some("café"));
    }
}
