//! Undo and redo for file operations: new files and folders, renames, moves,
//! duplicates, deletes and copies dropped on the file tree.
//!
//! Each entry keeps what an operation did (a `Change`). Undoing an entry does its inverse and stores what that did in
//! its place, so redoing inverts that again: undoing `Created(a)` moves `a` to
//! the Trash and stores `Trashed(a)`, and redoing that restores it. Deleting
//! always goes through the Trash, so it can be undone too.

use crate::fs::PathExt as _;
use std::{
    io,
    path::{Path, PathBuf},
};

/// How many operations are kept.
const LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub enum Change {
    Created(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    Trashed(Trashed),
    Restored(PathBuf),
    /// Several changes made as one operation (undone in reverse).
    Batch(Vec<Change>),
}

/// Something in the Trash, with what's needed to put it back.
#[derive(Clone, Debug)]
pub struct Trashed {
    pub original: PathBuf,
    /// A remote host's trash: where it went there (under the same mount).
    remote: Option<PathBuf>,
    /// Where macOS put it.
    #[cfg(target_os = "macos")]
    location: PathBuf,
    #[cfg(not(target_os = "macos"))]
    item: Option<trash::TrashItem>,
}

impl Change {
    /// Does the inverse of this change; returns what it did.
    pub fn invert(self) -> io::Result<Change> {
        match self {
            Change::Created(path) | Change::Restored(path) => move_to_trash(&path).map(Change::Trashed),
            Change::Trashed(trashed) => {
                restore(&trashed)?;
                Ok(Change::Restored(trashed.original))
            }
            Change::Renamed { from, to } => {
                if !to.exists_on_host() {
                    return Err(io::Error::other(format!("{} isn't there any more", to.display())));
                }
                if from.exists_on_host() {
                    return Err(io::Error::other(format!("{} exists again", from.display())));
                }
                if let Some(parent) = from.parent() {
                    crate::fs::create_dir_all(parent)?;
                }
                crate::fs::rename(&to, &from)?;
                Ok(Change::Renamed { from: to, to: from })
            }
            Change::Batch(changes) => {
                let mut done = Vec::new();
                for change in changes.into_iter().rev() {
                    done.push(change.invert()?);
                }
                Ok(Change::Batch(done))
            }
        }
    }

    /// Paths this change put somewhere (for showing them), or took away
    /// (for closing their tabs).
    pub fn visit(&self, f: &mut impl FnMut(&Change)) {
        match self {
            Change::Batch(changes) => changes.iter().for_each(|c| c.visit(f)),
            change => f(change),
        }
    }
}

/// Moves `path` to the Trash in a way that can be restored.
pub fn move_to_trash(path: &Path) -> io::Result<Trashed> {
    if let Some(location) = crate::fs::trash_remote(path) {
        return Ok(Trashed {
            original: path.to_path_buf(),
            remote: Some(location?),
            #[cfg(target_os = "macos")]
            location: PathBuf::new(),
            #[cfg(not(target_os = "macos"))]
            item: None,
        });
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSFileManager, NSString, NSURL};
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let mut resulting = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|e| io::Error::other(e.localizedDescription().to_string()))?;
        let location = resulting.and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string()));
        let location = location.ok_or_else(|| io::Error::other("the Trash didn't say where it went"))?;
        Ok(Trashed { original: path.to_path_buf(), remote: None, location })
    }
    #[cfg(not(target_os = "macos"))]
    {
        trash::delete(path).map_err(|e| io::Error::other(e.to_string()))?;
        // The newest item from that path is the one just trashed.
        let item = trash::os_limited::list()
            .ok()
            .and_then(|items| items.into_iter().filter(|i| i.original_path() == path).max_by_key(|i| i.time_deleted));
        Ok(Trashed { original: path.to_path_buf(), remote: None, item })
    }
}

fn restore(trashed: &Trashed) -> io::Result<()> {
    if trashed.original.exists_on_host() {
        return Err(io::Error::other(format!("{} exists again", trashed.original.display())));
    }
    if let Some(parent) = trashed.original.parent() {
        crate::fs::create_dir_all(parent)?;
    }
    if let Some(location) = &trashed.remote {
        return crate::fs::rename(location, &trashed.original);
    }
    #[cfg(target_os = "macos")]
    {
        crate::fs::rename(&trashed.location, &trashed.original)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let item = trashed.item.clone().ok_or_else(|| io::Error::other("it isn't in the Trash"))?;
        trash::os_limited::restore_all([item]).map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Operations with a cursor: entries before it can be undone, the one at it
/// redone. Each entry keeps the name of the operation for menus and toasts,
/// and an id that stays with it (a toast's Undo button checks it's still next).
#[derive(Default)]
pub struct FileHistory {
    entries: Vec<Entry>,
    cursor: usize,
    next_id: u64,
}

struct Entry {
    id: u64,
    label: String,
    change: Change,
}

impl FileHistory {
    /// Records an operation that just happened; forgets what was undone.
    /// Returns its id.
    pub fn record(&mut self, label: impl Into<String>, change: Change) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.truncate(self.cursor);
        self.entries.push(Entry { id, label: label.into(), change });
        if self.entries.len() > LIMIT {
            self.entries.remove(0);
        }
        self.cursor = self.entries.len();
        id
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.cursor.checked_sub(1).map(|ix| self.entries[ix].label.as_str())
    }

    /// The id of the operation undo would undo.
    pub fn undo_id(&self) -> Option<u64> {
        self.cursor.checked_sub(1).map(|ix| self.entries[ix].id)
    }

    /// The change undo (or redo) would invert.
    pub fn next(&self, redo: bool) -> Option<&Change> {
        let ix = if redo { self.cursor } else { self.cursor.checked_sub(1)? };
        self.entries.get(ix).map(|entry| &entry.change)
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(|entry| entry.label.as_str())
    }

    /// Undoes (or redoes) the operation next to the cursor. On failure the
    /// entry is dropped: what's on disk no longer matches it.
    pub fn step(&mut self, redo: bool) -> Option<(String, io::Result<Change>)> {
        let ix = if redo { self.cursor } else { self.cursor.checked_sub(1)? };
        let entry = self.entries.get(ix)?;
        let label = entry.label.clone();
        match entry.change.clone().invert() {
            Ok(done) => {
                self.entries[ix].change = done.clone();
                self.cursor = if redo { ix + 1 } else { ix };
                Some((label, Ok(done)))
            }
            Err(err) => {
                self.entries.remove(ix);
                self.cursor = ix;
                Some((label, Err(err)))
            }
        }
    }
}

/// The highest folder that creating `path` would create, else `path`
/// itself: undoing removes all of it.
pub fn topmost_new(path: &Path) -> PathBuf {
    let mut top = path.to_path_buf();
    for dir in path.ancestors().skip(1) {
        if dir.exists_on_host() {
            break;
        }
        top = dir.to_path_buf();
    }
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_undo_and_redo() {
        let dir = std::env::temp_dir().join(format!("kuana-file-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "x").unwrap();
        std::fs::rename(&a, &b).unwrap();

        let mut history = FileHistory::default();
        let id = history.record("Rename a.txt", Change::Renamed { from: a.clone(), to: b.clone() });
        assert_eq!((history.undo_label(), history.undo_id()), (Some("Rename a.txt"), Some(id)));
        assert!(history.step(false).unwrap().1.is_ok());
        assert!(a.exists() && !b.exists());
        assert_eq!((history.undo_label(), history.redo_label(), history.undo_id()), (None, Some("Rename a.txt"), None));
        assert!(history.step(true).unwrap().1.is_ok());
        assert!(b.exists() && !a.exists());
        // Redone, it's the same operation, with the same id.
        assert_eq!(history.undo_id(), Some(id));

        // Something else took the name back: undoing fails and is dropped.
        std::fs::write(&a, "new").unwrap();
        assert!(history.step(false).unwrap().1.is_err());
        assert_eq!(history.undo_label(), None);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Uses the real Trash: `cargo test -- --ignored trash_round_trip`.
    #[test]
    #[ignore]
    fn trash_round_trip() {
        let dir = std::env::temp_dir().join(format!("kuana-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gone.txt");
        std::fs::write(&file, "keep me").unwrap();
        let mut history = FileHistory::default();
        history.record("Delete", Change::Trashed(move_to_trash(&file).unwrap()));
        assert!(!file.exists());
        assert!(history.step(false).unwrap().1.is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
        assert!(history.step(true).unwrap().1.is_ok());
        assert!(!file.exists());
        // Back once more so nothing is left in the Trash.
        assert!(history.step(false).unwrap().1.is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_folders_a_path_creates() {
        let dir = std::env::temp_dir();
        assert_eq!(topmost_new(&dir.join("kuana-no-such/x/y.txt")), dir.join("kuana-no-such"));
        assert_eq!(topmost_new(&dir.join("kuana-y.txt")), dir.join("kuana-y.txt"));
    }
}
