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
            let Ok(meta) = crate::fs::metadata(path) else { continue };
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

/// The project's files across its folders (`project_folders::Project`):
/// each folder's own scan, kept and updated apart (a change, or new ignore
/// rules, in one folder rereads only that folder), offered as one list.
#[derive(Clone, Default)]
pub struct ProjectFiles {
    /// In the project's folder order.
    folders: Vec<Arc<ProjectSnapshot>>,
    /// Every folder's openable files, in folder order.
    openable: Arc<Vec<PathBuf>>,
}

impl ProjectFiles {
    /// The scans of `folders` that `self` has, in that order; folders not
    /// scanned yet are left out until `with` brings their scan.
    pub fn keeping(&self, folders: &[PathBuf]) -> Self {
        let kept = folders.iter().filter_map(|f| self.folders.iter().find(|s| s.root() == f.as_path()).cloned()).collect();
        Self::from_snapshots(kept)
    }

    /// With `snapshot` as its folder's scan (in place of an older one),
    /// ordered as `folders` says.
    pub fn with(&self, snapshot: ProjectSnapshot, folders: &[PathBuf]) -> Self {
        let mut all: Vec<Arc<ProjectSnapshot>> = self.folders.iter().filter(|s| s.root() != snapshot.root()).cloned().collect();
        all.push(Arc::new(snapshot));
        all.retain(|s| folders.iter().any(|f| f.as_path() == s.root()));
        all.sort_by_key(|s| folders.iter().position(|f| f.as_path() == s.root()));
        Self::from_snapshots(all)
    }

    fn from_snapshots(folders: Vec<Arc<ProjectSnapshot>>) -> Self {
        let openable = match folders.as_slice() {
            [one] => one.openable(),
            many => Arc::new(many.iter().flat_map(|s| s.openable().iter().cloned().collect::<Vec<_>>()).collect()),
        };
        Self { folders, openable }
    }

    /// Whether every one of `folders` is scanned.
    pub fn covers(&self, folders: &[PathBuf]) -> bool {
        folders.iter().all(|f| self.folders.iter().any(|s| s.root() == f.as_path()))
    }

    /// The scan of the folder holding `path`.
    pub fn snapshot_of(&self, path: &Path) -> Option<&Arc<ProjectSnapshot>> {
        self.folders.iter().find(|s| path.starts_with(s.root()))
    }

    /// Files to offer in Go to File: each folder's, sorted, in folder order.
    pub fn openable(&self) -> Arc<Vec<PathBuf>> {
        self.openable.clone()
    }

    /// Files Find in Files reads: none that are ignored.
    pub fn searchable(&self) -> Vec<PathBuf> {
        self.folders.iter().flat_map(|s| s.searchable()).collect()
    }

    pub fn is_searchable(&self, path: &Path) -> bool {
        self.snapshot_of(path).is_some_and(|s| s.is_searchable(path))
    }

    pub fn lists(&self, path: &Path) -> bool {
        self.snapshot_of(path).is_some_and(|s| s.lists(path))
    }
}

/// Folders never read: Git's own, dependencies, and macOS's litter.
fn skipped(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| rel.components().any(|c| matches!(c.as_os_str().to_str(), Some(".git" | "node_modules" | ".DS_Store"))))
}

/// The files under `dir`, sorted, each with whether it's ignored (see `add_tree`).
pub fn scan_files(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut files = BTreeMap::new();
    add_tree(dir, &mut files);
    files.into_iter().collect()
}

/// The files under `dir`: in folders the ignore rules let in, every file,
/// marked when it's ignored itself. A remote host's are found there
/// (`kuana-server` runs `scan_files`), with its ignore rules.
fn add_tree(dir: &Path, files: &mut BTreeMap<PathBuf, bool>) {
    if let (Some((host, path)), Some(mount)) = (crate::fs::remote(dir), crate::fs::mount_of(dir)) {
        if let Some(Ok(found)) = host.scan(&path) {
            files.extend(found.into_iter().map(|(path, ignored)| (crate::fs::local_path(&mount, &path), ignored)));
        }
        return;
    }
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
        let Ok(entries) = crate::fs::read_dir(&dir) else { continue };
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

    #[test]
    fn several_folders_are_one_list() {
        let base = std::env::temp_dir().join(format!("kuana-project-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (api, web) = (base.join("api"), base.join("web"));
        for (dir, file) in [(&api, "main.rs"), (&web, "app.ts")] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src").join(file), "").unwrap();
        }
        let folders = vec![api.clone(), web.clone()];
        let files = ProjectFiles::default().with(ProjectSnapshot::scan(&web), &folders);
        assert!(!files.covers(&folders) && files.covers(std::slice::from_ref(&web)));
        // Scanned in any order, listed in the project's.
        let files = files.with(ProjectSnapshot::scan(&api), &folders);
        assert!(files.covers(&folders));
        assert_eq!(*files.openable(), [api.join("src/main.rs"), web.join("src/app.ts")]);
        assert_eq!(files.searchable().len(), 2);
        assert!(files.lists(&web.join("src/app.ts")) && files.is_searchable(&api.join("src/main.rs")));
        assert!(!files.lists(&base.join("elsewhere.rs")));
        assert_eq!(files.snapshot_of(&web.join("src/app.ts")).map(|s| s.root()), Some(web.as_path()));
        // A folder rescanned replaces its own scan only.
        std::fs::write(web.join("src/b.ts"), "").unwrap();
        let files = files.with(ProjectSnapshot::scan(&web), &folders);
        assert_eq!(files.openable().len(), 3);
        // A folder removed from the project goes with its files.
        let files = files.keeping(std::slice::from_ref(&api));
        assert_eq!(*files.openable(), [api.join("src/main.rs")]);
        std::fs::remove_dir_all(&base).ok();
    }
}
