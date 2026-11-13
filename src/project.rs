//! The project's files, scanned once in the background and then kept up to
//! date from the watcher's events (`apply`), instead of walking the project
//! again for each use. Go to File / Search Everywhere list `openable` (git-
//! ignored files too, but nothing inside ignored folders), Find in Files
//! searches `searchable` (not ignored).

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use ignore::{Match, gitignore::Gitignore};

#[derive(Clone, Default)]
pub struct ProjectSnapshot {
    root: PathBuf,
    /// Every file in a folder that isn't ignored, and whether it's ignored
    /// itself (a `.env` in the project folder is).
    files: BTreeMap<PathBuf, bool>,
    /// Each folder's own .gitignore (None: it has none), read as changes
    /// need them: a path is ignored by the rules of every folder above it,
    /// as in the scan, not only the project's.
    ignores: HashMap<PathBuf, Option<Arc<Gitignore>>>,
    /// `.git/info/exclude` and the user's global excludes, below them all.
    excludes: Vec<Arc<Gitignore>>,
    openable: Arc<Vec<PathBuf>>,
}

/// What a batch of changes did.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    Unchanged,
    Changed,
    /// The project's own ignore rules changed: scan again.
    Rescan,
}

impl ProjectSnapshot {
    /// Reads the whole project (slow on big ones: run it in the background).
    pub fn scan(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        add_tree(root, &mut files);
        // Both read relative to the project folder (their patterns are).
        let mut excludes = Vec::new();
        let mut exclude = ignore::gitignore::GitignoreBuilder::new(root);
        exclude.add(root.join(".git/info/exclude"));
        excludes.extend(exclude.build().ok().map(Arc::new));
        let (global, _) = ignore::gitignore::GitignoreBuilder::new(root).build_global();
        excludes.push(Arc::new(global));
        let mut snapshot = Self { root: root.to_path_buf(), files, ignores: HashMap::new(), excludes, openable: Arc::default() };
        snapshot.openable = Arc::new(snapshot.files.keys().cloned().collect());
        snapshot
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Files to offer in Go to File, sorted.
    pub fn openable(&self) -> Arc<Vec<PathBuf>> {
        self.openable.clone()
    }

    /// Files Find in Files reads, sorted: none that are ignored.
    pub fn searchable(&self) -> Vec<PathBuf> {
        self.files.iter().filter(|(_, ignored)| !**ignored).map(|(path, _)| path.clone()).collect()
    }

    /// Whether Find in Files reads `path` (listed and not ignored).
    pub fn is_searchable(&self, path: &Path) -> bool {
        self.files.get(path) == Some(&false)
    }

    /// Whether `path` is listed, ignored or not.
    pub fn lists(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    /// Takes in paths the watcher reported as created, removed or renamed:
    /// a file is added or dropped, a folder read (or dropped) with what's in it.
    pub fn apply<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> Applied {
        let mut changed = false;
        for path in paths {
            if !path.starts_with(&self.root) || path == self.root || skipped(&self.root, path) {
                continue;
            }
            if path.file_name().is_some_and(|n| n == ".gitignore") {
                let dir = path.parent().unwrap_or(&self.root).to_path_buf();
                // The project's own rules: everything may change.
                if dir == self.root {
                    return Applied::Rescan;
                }
                self.ignores.remove(&dir);
                // In a folder that's ignored anyway (a tool's cache): nothing changes.
                if self.ignored(&dir, true) {
                    continue;
                }
                // A folder's rules changed: only what's in it is read again.
                let under: Vec<PathBuf> = self.files.range(dir.clone()..).take_while(|(p, _)| p.starts_with(&dir)).map(|(p, _)| p.clone()).collect();
                for p in under {
                    self.files.remove(&p);
                }
                add_tree(&dir, &mut self.files);
                changed = true;
                continue;
            }
            // Gone (or renamed away): it and anything under it.
            let under: Vec<PathBuf> = self.files.range(path.to_path_buf()..).take_while(|(p, _)| p.starts_with(path)).map(|(p, _)| p.clone()).collect();
            changed |= !under.is_empty();
            for p in under {
                self.files.remove(&p);
            }
            let Ok(meta) = std::fs::metadata(path) else { continue };
            // In an ignored folder: Go to File doesn't list it.
            if path.parent().is_some_and(|parent| parent != self.root && self.ignored(parent, true)) {
                continue;
            }
            if meta.is_dir() {
                if !self.ignored(path, true) {
                    add_tree(path, &mut self.files);
                    changed = true;
                }
            } else if meta.is_file() && path.file_name().is_some_and(|n| n != ".DS_Store") {
                let ignored = self.ignored(path, false);
                self.files.insert(path.to_path_buf(), ignored);
                changed = true;
            }
        }
        if changed {
            self.openable = Arc::new(self.files.keys().cloned().collect());
            Applied::Changed
        } else {
            Applied::Unchanged
        }
    }

    /// Whether Git ignores `path`, by the .gitignore files of the folders
    /// above it (the nearest decides), then `.git/info/exclude` and the
    /// global excludes, as Git (and the scan) see it.
    fn ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let root = self.root.clone();
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&root)) {
            let ignore = self.ignores.entry(d.to_path_buf()).or_insert_with(|| {
                let file = d.join(".gitignore");
                file.is_file().then(|| Arc::new(Gitignore::new(file).0))
            });
            if let Some(ignore) = ignore {
                match ignore.matched_path_or_any_parents(path, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
            dir = d.parent();
        }
        self.excludes.iter().any(|i| i.matched_path_or_any_parents(path, is_dir).is_ignore())
    }
}

/// Folders never read: Git's own, dependencies, and macOS's litter.
fn skipped(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.components().any(|c| matches!(c.as_os_str().to_str(), Some(".git" | "node_modules" | ".DS_Store"))))
}

/// The files under `dir`: in folders the ignore rules let in, every file,
/// marked when it's ignored itself.
fn add_tree(dir: &Path, files: &mut BTreeMap<PathBuf, bool>) {
    let mut walked = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    for entry in walker(dir).filter_map(Result::ok) {
        match entry.file_type() {
            Some(t) if t.is_dir() => dirs.push(entry.into_path()),
            Some(t) if t.is_file() => {
                walked.insert(entry.into_path());
            }
            _ => {}
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) && entry.file_name() != ".DS_Store" {
                let path = entry.path();
                let ignored = !walked.contains(&path);
                files.insert(path, ignored);
            }
        }
    }
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name();
            name != ".git" && name != "node_modules" && name != ".DS_Store"
        })
        .build()
}

/// The project's .gitignore and .git/info/exclude.
pub fn project_ignore(root: &Path) -> Option<Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git/info/exclude"));
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(files: &[PathBuf], root: &Path) -> Vec<String> {
        files.iter().map(|p| p.strip_prefix(root).unwrap().display().to_string().replace('\\', "/")).collect()
    }

    #[test]
    fn keeps_up_with_changes_without_scanning_again() {
        let root = std::env::temp_dir().join(format!("kuana-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        // .gitignore counts in a Git repository.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n.env\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("build/out.o"), "").unwrap();
        let mut snapshot = ProjectSnapshot::scan(&root);
        // Ignored files are openable, not searchable; ignored folders neither.
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "src/a.rs"]);
        assert_eq!(names(&snapshot.searchable(), &root), [".gitignore", "src/a.rs"]);

        // A new folder with files, a new file, a removed one.
        std::fs::create_dir_all(root.join("lib/deep")).unwrap();
        std::fs::write(root.join("lib/deep/b.rs"), "").unwrap();
        std::fs::write(root.join("src/c.rs"), "").unwrap();
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        std::fs::write(root.join("build/more.o"), "").unwrap();
        let changed = [root.join("lib"), root.join("src/c.rs"), root.join("src/a.rs"), root.join("build/more.o")];
        assert_eq!(snapshot.apply(changed.iter().map(PathBuf::as_path)), Applied::Changed);
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "lib/deep/b.rs", "src/c.rs"]);

        // A folder renamed away takes its files along.
        std::fs::rename(root.join("lib"), root.join("vendor")).unwrap();
        let moved = [root.join("lib"), root.join("vendor")];
        snapshot.apply(moved.iter().map(PathBuf::as_path));
        assert_eq!(names(&snapshot.openable(), &root), [".env", ".gitignore", "src/c.rs", "vendor/deep/b.rs"]);

        // A folder's own .gitignore counts for what's created in it later
        // (build output in a package of a monorepo).
        std::fs::create_dir_all(root.join("pkg/dist")).unwrap();
        std::fs::write(root.join("pkg/.gitignore"), "dist/\n").unwrap();
        std::fs::write(root.join("pkg/index.js"), "").unwrap();
        std::fs::write(root.join("pkg/dist/bundle.js"), "").unwrap();
        snapshot.apply([root.join("pkg")].iter().map(PathBuf::as_path));
        std::fs::create_dir_all(root.join("pkg/dist/chunks")).unwrap();
        std::fs::write(root.join("pkg/dist/chunks/1.js"), "").unwrap();
        snapshot.apply([root.join("pkg/dist/chunks"), root.join("pkg/dist/chunks/1.js")].iter().map(PathBuf::as_path));
        assert_eq!(names(&snapshot.searchable(), &root), [".gitignore", "pkg/.gitignore", "pkg/index.js", "src/c.rs", "vendor/deep/b.rs"]);

        // A nested .gitignore changing reads only its folder again.
        std::fs::write(root.join("pkg/.gitignore"), "").unwrap();
        assert_eq!(snapshot.apply([root.join("pkg/.gitignore")].iter().map(PathBuf::as_path)), Applied::Changed);
        assert!(names(&snapshot.searchable(), &root).contains(&"pkg/dist/chunks/1.js".to_string()));

        // New project-wide ignore rules mean scanning again.
        let gitignore = [root.join(".gitignore")];
        assert_eq!(snapshot.apply(gitignore.iter().map(PathBuf::as_path)), Applied::Rescan);
        std::fs::remove_dir_all(&root).ok();
    }
}
