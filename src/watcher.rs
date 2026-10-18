//! Watches a project folder (FSEvents on macOS, inotify on Linux,
//! ReadDirectoryChanges on Windows, via `notify`) so the file tree, Go to File,
//! Git status and open files follow changes made outside Kuana: a checkout, a
//! code generator, `npm install`. Events arrive on a background thread; the
//! workspace batches them and decides what each one affects.

use std::path::{Component, Path, PathBuf};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use notify::{EventKind, RecursiveMode, Watcher as _, event::ModifyKind};

/// A changed path. `structural`: created, removed or renamed (as opposed to
/// written to), so folder listings may differ.
#[derive(Clone, Debug, PartialEq)]
pub struct FsChange {
    pub path: PathBuf,
    pub structural: bool,
}

pub struct Watcher {
    inner: notify::RecommendedWatcher,
}

impl Watcher {
    /// Starts watching `root` recursively; changes come out of the receiver.
    /// When the system lost track (its queue overflowed, it asks for a
    /// rescan), the batch holds `root` itself: anything may have changed.
    pub fn new(root: &Path) -> notify::Result<(Watcher, UnboundedReceiver<Vec<FsChange>>)> {
        let (tx, rx) = unbounded();
        let everything = root.to_path_buf();
        let mut inner = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let event = match event {
                Ok(event) if !event.need_rescan() => event,
                // Events were lost: look at everything again
                // (rather than polling Git all the time to catch this).
                _ => {
                    let _ = tx.unbounded_send(vec![FsChange { path: everything.clone(), structural: true }]);
                    return;
                }
            };
            let structural = match event.kind {
                // Reads and attribute changes don't change anything shown.
                EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => return,
                EventKind::Modify(ModifyKind::Data(_)) => false,
                // Create, Remove, renames, and the unspecific kinds some
                // platforms report: assume the listing changed.
                _ => true,
            };
            let changes: Vec<FsChange> = event.paths.into_iter().map(|path| FsChange { path, structural }).collect();
            if !changes.is_empty() {
                let _ = tx.unbounded_send(changes);
            }
        })?;
        inner.watch(root, RecursiveMode::Recursive)?;
        Ok((Watcher { inner }, rx))
    }

    /// Also watches `path` (a repository's `.git` outside the project folder).
    pub fn add(&mut self, path: &Path) -> notify::Result<()> {
        self.inner.watch(path, RecursiveMode::Recursive)
    }
}

/// What a change inside a `.git` folder means.
#[derive(Debug, PartialEq, Eq)]
pub enum GitPath {
    /// Not in a `.git` folder.
    WorkTree,
    /// HEAD, the index, refs or merge/rebase state moved: Git status (and the
    /// log) may differ.
    Metadata,
    /// Objects, logs, lock files and the like: nothing to refresh.
    Internal,
}

pub fn git_path(path: &Path) -> GitPath {
    let mut components = path.components();
    if !components.any(|c| c.as_os_str() == ".git") {
        return GitPath::WorkTree;
    }
    let rest: Vec<String> = components.map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match rest.first().map(String::as_str) {
        // The .git folder itself (some platforms report the parent of a change).
        None => GitPath::Metadata,
        Some(name) if name.ends_with(".lock") => GitPath::Internal,
        Some("HEAD" | "index" | "ORIG_HEAD" | "MERGE_HEAD" | "REBASE_HEAD" | "CHERRY_PICK_HEAD" | "FETCH_HEAD" | "packed-refs") => {
            GitPath::Metadata
        }
        Some("refs" | "rebase-merge" | "rebase-apply") if !rest.last().is_some_and(|n| n.ends_with(".lock")) => GitPath::Metadata,
        _ => GitPath::Internal,
    }
}

/// The outermost folder named like build output or dependencies (`target`,
/// `node_modules`) that `path` is in, under `root`. Their churn (builds,
/// package installs) matters to the file tree only, when they're ignored as
/// such folders nearly always are; a `target` package in a Java project is
/// source like any other (`workspace::watching::classify` checks).
pub fn heavy_folder(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    let mut folder = root.to_path_buf();
    for component in relative.components() {
        folder.push(component);
        if matches!(component, Component::Normal(name) if name == "target" || name == "node_modules") {
            return Some(folder);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_paths_are_sorted_by_what_they_affect() {
        assert_eq!(git_path(Path::new("/p/src/main.rs")), GitPath::WorkTree);
        assert_eq!(git_path(Path::new("/p/.gitignore")), GitPath::WorkTree);
        assert_eq!(git_path(Path::new("/p/.git")), GitPath::Metadata);
        assert_eq!(git_path(Path::new("/p/.git/HEAD")), GitPath::Metadata);
        assert_eq!(git_path(Path::new("/p/.git/index")), GitPath::Metadata);
        assert_eq!(git_path(Path::new("/p/.git/refs/heads/main")), GitPath::Metadata);
        assert_eq!(git_path(Path::new("/p/.git/index.lock")), GitPath::Internal);
        assert_eq!(git_path(Path::new("/p/.git/refs/heads/main.lock")), GitPath::Internal);
        assert_eq!(git_path(Path::new("/p/.git/objects/ab/cdef")), GitPath::Internal);
        assert_eq!(git_path(Path::new("/p/.git/logs/HEAD")), GitPath::Internal);
        let heavy = |path: &str| heavy_folder(Path::new("/p"), Path::new(path));
        assert_eq!(heavy("/p/target/debug/kuana"), Some(PathBuf::from("/p/target")));
        assert_eq!(heavy("/p/web/node_modules/x/node_modules/y.js"), Some(PathBuf::from("/p/web/node_modules")));
        assert_eq!(heavy("/p/src/targets.rs"), None);
        assert_eq!(heavy("/elsewhere/target/x"), None);
    }

    #[test]
    fn reports_created_and_written_files() {
        let dir = std::env::temp_dir().join(format!("kuana-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (_watcher, mut rx) = Watcher::new(&dir).unwrap();
        // FSEvents may deliver events from just before the watch started.
        std::thread::sleep(std::time::Duration::from_millis(200));
        while rx.try_recv().is_ok() {}
        let file = dir.join("new.txt");
        std::fs::write(&file, "x").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline && !seen.iter().any(|c: &FsChange| c.path == file && c.structural) {
            match rx.try_recv() {
                Ok(changes) => seen.extend(changes),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        assert!(seen.iter().any(|c| c.path == file && c.structural), "{seen:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
