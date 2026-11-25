//! Watching the project folder: what a change on disk updates. Each batch
//! of changes is sorted out in the background (`classify`: it stats paths,
//! matches ignore rules and language servers' globs), so churn in build
//! output or a venv costs the UI thread a short summary, not a look at
//! every path.

use ignore::gitignore::Gitignore;

use super::*;
use crate::lsp::{FileWatchers, ServerKey};

impl Workspace {
    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project's folders so changes made outside Kuana show up right away.
    pub(super) fn start_watching(&mut self, cx: &mut Context<Self>) {
        self.watcher = None;
        self.watch_task = None;
        let Some(project) = self.folders.clone() else { return };
        self.ignores = project.folders().iter().filter_map(|f| Some((f.clone(), Arc::new(project_ignore(f)?)))).collect();
        let (mut watcher, mut changes) = match Watcher::new(project.primary()) {
            Ok(watching) => watching,
            Err(err) => {
                eprintln!("couldn't watch {}: {err}", project.primary().display());
                return;
            }
        };
        for folder in &project.folders()[1..] {
            if let Err(err) = watcher.add(folder) {
                eprintln!("couldn't watch {}: {err}", folder.display());
            }
        }
        self.watcher = Some(watcher);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(mut batch) = changes.next().await {
                // A burst (a checkout, npm install) becomes one update.
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while let Ok(more) = changes.try_recv() {
                    batch.extend(more);
                }
                let Ok(state) = this.update(cx, |ws, cx| ws.watch_state(cx)) else { break };
                let Some(state) = state else { continue };
                let changed = cx.background_executor().spawn(async move { classify(batch, &state) }).await;
                if this.update(cx, |ws, cx| ws.disk_changed(changed, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Whether the project's own ignore rules (its folder's .gitignore) ignore `path`.
    pub(super) fn project_ignores(&self, path: &Path, is_dir: bool) -> bool {
        self.searched_files().ignores(path, is_dir)
    }

    /// Which files Find in Files reads, as things are now, to take along
    /// where the workspace can't be read (a list being changed).
    pub(super) fn searched_files(&self) -> SearchedFiles {
        SearchedFiles { files: self.files.clone(), project: self.folders.clone(), ignores: self.ignores.clone() }
    }

    /// What sorting out a batch of changes goes by, as it is now.
    fn watch_state(&self, cx: &App) -> Option<WatchState> {
        let folders = self.folders.as_ref()?.folders().iter().map(|f| (f.clone(), self.ignores.get(f).cloned())).collect();
        let mut open: HashSet<PathBuf> = self.shown_editors(cx).iter().filter_map(|e| e.read(cx).doc.read(cx).path.clone()).collect();
        open.extend(self.images(cx).iter().map(|i| i.read(cx).path().to_path_buf()));
        open.extend(self.diff_views(cx).iter().flat_map(|d| d.read(cx).disk_files().into_iter().map(Path::to_path_buf).collect::<Vec<_>>()));
        Some(WatchState {
            folders,
            git_ignored: self.all_git_paths(cx).1,
            tree_read: self.tree.read(cx).read_folders(),
            open,
            watchers: self.lsp.read(cx).file_watchers(),
        })
    }

    /// Updates what a batch of changes on disk affects (`classify` sorted
    /// it out): the tree (in folders it has read), Go to File, Git status,
    /// language servers and open files.
    pub(super) fn disk_changed(&mut self, changed: Classified, cx: &mut Context<Self>) {
        let Some(project) = self.folders.clone() else { return };
        for folder in project.folders() {
            let gone = !folder.is_dir_on_host();
            if gone == self.folders_gone.contains(folder) {
                continue;
            }
            if gone {
                self.folders_gone.insert(folder.clone());
                self.toast(ToastKind::Error, "A project folder was deleted", Some(clone_dialog::display_path(folder)), cx);
            } else {
                self.folders_gone.remove(folder);
            }
            // Gone: the branch, changes and log go with it (no repository is
            // found there); back: they're read again.
            if folder.as_path() == project.primary() {
                self.detect_repo(folder.clone(), cx);
            }
        }
        // A venv may have been created or removed.
        self.venv_shown.take();
        // The watcher lost events: everything may have changed.
        if changed.everything {
            self.tree.update(cx, |t, cx| t.relist_all(cx));
            self.refresh_file_index(cx);
            self.refresh_git(cx);
            self.reload_documents(|_| true, cx);
            self.tests_files_changed(project.folders().to_vec(), cx);
            self.todo_files_changed(project.folders().to_vec(), cx);
            cx.notify();
            return;
        }
        if !changed.files.is_empty() {
            self.tests_files_changed(changed.files.iter().cloned(), cx);
            self.todo_files_changed(changed.files, cx);
        }
        for (folder, ignore) in changed.ignores {
            match ignore {
                Some(ignore) => self.ignores.insert(folder, ignore),
                None => self.ignores.remove(&folder),
            };
        }
        if !changed.tree.is_empty() {
            self.tree.update(cx, |t, cx| t.paths_changed(changed.tree.iter().map(PathBuf::as_path), cx));
        }
        if !changed.structural.is_empty() {
            self.update_project(changed.structural, cx);
        }
        if !changed.lsp.is_empty() {
            self.lsp.read(cx).send_file_events(changed.lsp);
        }
        if changed.project_settings {
            self.check_config(cx);
        }
        self.read_status(changed.git, cx);
        let paths = changed.open;
        if paths.is_empty() {
            return;
        }
        self.reload_documents(|path| paths.contains(path), cx);
        for image in self.images(cx) {
            if paths.contains(image.read(cx).path()) {
                image.update(cx, |i, cx| i.reload_if_changed(cx));
            }
        }
        // An edit from outside (a formatter in the terminal, another editor)
        // that leaves Git's status as it was: the diff of the file shows it.
        for diff in self.diff_views(cx) {
            if diff.read(cx).disk_files().iter().any(|p| paths.contains(*p)) {
                diff.update(cx, |v, cx| v.reload(cx));
            }
        }
    }

    /// Has the documents whose files `wanted` picks read them again (in the
    /// background), each through one of its tabs: splits share it.
    pub(super) fn reload_documents(&mut self, wanted: impl Fn(&Path) -> bool, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        // A file only an excerpt shows reloads through it.
        for editor in self.shown_editors(cx) {
            let doc = editor.read(cx).doc.read(cx);
            if editor.read(cx).is_loaded() && doc.path.as_deref().is_some_and(&wanted) && seen.insert(doc.id()) {
                editor.update(cx, |e, cx| e.reload_if_changed(cx));
            }
        }
    }

    /// Scans every project folder's files in the background (a new
    /// project, lost watcher events); changes waiting meanwhile are applied after.
    pub(super) fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.folders.clone() else { return };
        self.project_changes.clear();
        self.scan_folders(project.folders().to_vec(), cx);
    }

    /// Scans `folders` again (new ignore rules in one), in the background.
    fn scan_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let scans = cx.background_executor().spawn(async move { folders.iter().map(|f| ProjectSnapshot::scan(f)).collect::<Vec<_>>() }).await;
            this.update(cx, |ws, cx| {
                let Some(project) = ws.folders.clone() else { return };
                let files = scans.into_iter().fold(ws.files.clone(), |files, scan| files.with(scan, project.folders()));
                ws.project_updated(files, cx);
            })
            .ok();
        }));
    }

    /// Paths created, removed or renamed on disk: the project's files catch
    /// up in the background, without scanning everything again.
    pub(super) fn update_project(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.project_changes.extend(paths);
        if self.project_task.is_none() {
            self.apply_project_changes(cx);
        }
    }

    fn apply_project_changes(&mut self, cx: &mut Context<Self>) {
        if self.project_changes.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.project_changes);
        // Each folder's scan takes its own paths.
        let mut by_folder: Vec<(Arc<ProjectSnapshot>, Vec<PathBuf>)> = Vec::new();
        for path in paths {
            let Some(scan) = self.files.snapshot_of(&path) else { continue };
            match by_folder.iter_mut().find(|(s, _)| Arc::ptr_eq(s, scan)) {
                Some((_, list)) => list.push(path),
                None => by_folder.push((scan.clone(), vec![path])),
            }
        }
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    by_folder
                        .into_iter()
                        .map(|(base, paths)| {
                            let mut scan = (*base).clone();
                            let applied = scan.apply(paths.iter().map(PathBuf::as_path));
                            (scan, applied)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |ws, cx| {
                let Some(project) = ws.folders.clone() else { return };
                let mut files = ws.files.clone();
                let (mut changed, mut rescan) = (false, Vec::new());
                for (scan, applied) in results {
                    match applied {
                        Applied::Rescan => rescan.push(scan.root().to_path_buf()),
                        Applied::Changed => {
                            files = files.with(scan, project.folders());
                            changed = true;
                        }
                        Applied::Unchanged => {}
                    }
                }
                if !rescan.is_empty() {
                    if changed {
                        ws.files = files;
                    }
                    ws.scan_folders(rescan, cx);
                } else if changed {
                    ws.project_updated(files, cx);
                } else {
                    ws.project_updated_unchanged(cx);
                }
            })
            .ok();
        }));
    }

    fn project_updated(&mut self, files: ProjectFiles, cx: &mut Context<Self>) {
        // The tree's search looks through the files too. It lets go of the
        // old ones first, so they're freed with the old scans.
        self.tree.update(cx, |t, cx| t.set_files(files.openable(), cx));
        // Freeing a big project's paths takes a while: not on the UI thread.
        let old = std::mem::replace(&mut self.files, files);
        cx.background_executor().spawn(async move { drop(old) }).detach();
        if let Some(Modal::Picker(picker)) = &self.modals.current {
            let files = self.files.openable();
            picker.update(cx, |p, cx| p.set_files(files, cx));
        }
        let openable = self.files.openable();
        for view in self.items(cx).into_iter().filter_map(|i| i.agent().cloned()) {
            view.update(cx, |v, cx| v.set_files(openable.clone(), cx));
        }
        self.todo_project_listed(cx);
        self.tests_project_listed(cx);
        self.project_updated_unchanged(cx);
    }

    /// The running update is done: the next one, if changes came meanwhile.
    fn project_updated_unchanged(&mut self, cx: &mut Context<Self>) {
        // This task is running; let it finish instead of dropping it.
        if let Some(task) = self.project_task.take() {
            task.detach();
        }
        self.apply_project_changes(cx);
    }
}

/// Which files Find in Files reads (`Workspace::searched_files`).
pub(super) struct SearchedFiles {
    files: ProjectFiles,
    project: Option<Project>,
    ignores: HashMap<PathBuf, Arc<Gitignore>>,
}

impl SearchedFiles {
    /// Whether its folder's own rules ignore `path`.
    fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        let folder = self.project.as_ref().and_then(|p| p.folder_of(path));
        folder.and_then(|f| self.ignores.get(f)).is_some_and(|i| i.matched_path_or_any_parents(path, is_dir).is_ignore())
    }

    /// Whether Find in Files reads `path`: listed and not ignored, or new
    /// since the last scan, a file, and not ignored by its folder's rules.
    pub(super) fn reads(&self, path: &Path) -> bool {
        self.files.is_searchable(path) || (!self.files.lists(path) && path.is_file_on_host() && !self.ignores(path, false))
    }
}

/// What a batch of changes is sorted out by (`classify`), taken from the
/// workspace when it comes.
pub(super) struct WatchState {
    /// The project's folders, each with its own ignore rules.
    folders: Vec<(PathBuf, Option<Arc<Gitignore>>)>,
    /// What Git reported as ignored.
    git_ignored: Arc<HashSet<PathBuf>>,
    /// The folders the tree has read (or is reading).
    tree_read: HashSet<PathBuf>,
    /// Open files: tabs, pictures, the diff view's.
    open: HashSet<PathBuf>,
    watchers: FileWatchers,
}

/// What a batch of changes affects.
#[derive(Default)]
pub(super) struct Classified {
    /// The watcher lost track: anything may have changed.
    everything: bool,
    /// Created, removed or renamed in folders the tree has read.
    tree: Vec<PathBuf>,
    /// Created, removed or renamed in the project, for Go to File (which
    /// skips what's in ignored folders itself).
    structural: Vec<PathBuf>,
    /// Project folders' ignore rules, read again (their .gitignore changed).
    ignores: Vec<(PathBuf, Option<Arc<Gitignore>>)>,
    /// Git status to read again: all of it (`.git` changed), or paths'.
    git: StatusWork,
    /// Open files that changed.
    open: HashSet<PathBuf>,
    /// The project's paths that changed in any way (not in ignored build
    /// output or dependencies), for the TODO list.
    files: Vec<PathBuf>,
    /// Each language server's `didChangeWatchedFiles`, for what it watches.
    lsp: Vec<(ServerKey, serde_json::Value)>,
    /// A project settings file changed (a checkout, another editor).
    project_settings: bool,
}

/// Sorts out a batch of changes (blocking: in the background). Paths in a
/// `target` or `node_modules` folder that's ignored matter to the tree (and
/// open files) only.
pub(super) fn classify(changes: Vec<FsChange>, state: &WatchState) -> Classified {
    let mut out = Classified::default();
    if changes.iter().any(|c| state.folders.iter().any(|(f, _)| *f == c.path)) {
        out.everything = true;
        return out;
    }
    // A folder's new rules count for the whole batch.
    let mut ignores: Vec<(PathBuf, Option<Arc<Gitignore>>)> = state.folders.clone();
    for (folder, ignore) in &mut ignores {
        let rules_changed = changes.iter().any(|c| {
            c.path.parent() == Some(folder.as_path()) && c.path.file_name().is_some_and(|n| n == ".gitignore") && watcher::git_path(&c.path) == GitPath::WorkTree
        });
        if rules_changed {
            *ignore = project_ignore(folder).map(Arc::new);
            out.ignores.push((folder.clone(), ignore.clone()));
        }
    }
    // The project folder holding a path, with its rules as they are now.
    let folder_of = |path: &Path| ignores.iter().find(|(f, _)| path.starts_with(f)).map(|(f, i)| (f.as_path(), i.as_deref()));
    // Each path's kind of change, found once, and only when needed.
    let mut found: HashMap<PathBuf, Option<bool>> = HashMap::new();
    let mut kind_of = |change: &FsChange| {
        let is_dir = *found.entry(change.path.clone()).or_insert_with(|| crate::fs::metadata(&change.path).ok().map(|m| m.is_dir()));
        let kind = match (is_dir, change.structural) {
            (None, _) => FileChange::Deleted,
            (Some(_), true) => FileChange::Created,
            (Some(_), false) => FileChange::Changed,
        };
        (kind, is_dir == Some(true))
    };
    let mut file_changes = Vec::new();
    out.project_settings = changes.iter().any(|c| crate::project_settings::is_settings_file(&c.path) || c.path.ends_with(crate::project_settings::DIR));
    for change in changes {
        match watcher::git_path(&change.path) {
            GitPath::Metadata => out.git.add(StatusWork::all()),
            GitPath::Internal => {}
            GitPath::WorkTree => {
                let tree_read = |path: &Path| state.tree_read.contains(path);
                if change.structural && (change.path.parent().is_some_and(tree_read) || tree_read(&change.path)) {
                    out.tree.push(change.path.clone());
                }
                let Some((folder, ignore)) = folder_of(&change.path) else { continue };
                // Generated code a tab shows (a build script's output) is read again.
                if state.open.contains(&change.path) {
                    out.open.insert(change.path.clone());
                }
                if in_ignored_heavy_folder(&change.path, folder, state, ignore) {
                    continue;
                }
                out.files.push(change.path.clone());
                if change.structural {
                    out.structural.push(change.path.clone());
                }
                if let Some(path) = status_path(&change, folder, state, ignore, &mut kind_of) {
                    out.git.add_path(path);
                }
                if !state.watchers.is_empty() {
                    file_changes.push((change.path.clone(), kind_of(&change).0));
                }
            }
        }
    }
    out.lsp = state.watchers.events(&file_changes);
    out
}

/// Whether `path` is in a `target` or `node_modules` folder that its
/// project folder's rules or Git ignore: build output and dependencies,
/// whose churn is skipped without a look at each path. One that isn't
/// ignored (a Java package named `target`) is the project's like any other.
fn in_ignored_heavy_folder(path: &Path, root: &Path, state: &WatchState, ignore: Option<&Gitignore>) -> bool {
    let Some(heavy) = watcher::heavy_folder(root, path) else { return false };
    heavy.ancestors().take_while(|a| a.starts_with(root)).any(|a| state.git_ignored.contains(a))
        || ignore.is_some_and(|i| i.matched_path_or_any_parents(&heavy, true).is_ignore())
}

/// What of Git status a change in the work tree needs read again: its
/// path's; nothing in a folder Git ignores (build output churning there),
/// nor for an ignored file written to, unless it's open (it may be tracked
/// all the same); for something new that the project's rules ignore, the
/// outermost folder they ignore (a build's first output is read as its
/// folder, not file by file: once Git says it's ignored, the tree dims
/// it); for a .gitignore, its folder's.
fn status_path(
    change: &FsChange,
    root: &Path,
    state: &WatchState,
    ignore: Option<&Gitignore>,
    kind_of: &mut impl FnMut(&FsChange) -> (FileChange, bool),
) -> Option<PathBuf> {
    let path = &change.path;
    if path.ancestors().skip(1).any(|p| state.git_ignored.contains(p)) {
        return None;
    }
    if state.git_ignored.contains(path) {
        return (kind_of(change).0 == FileChange::Deleted).then(|| path.clone());
    }
    if path.file_name().is_some_and(|n| n == ".gitignore") {
        return path.parent().map(Path::to_path_buf);
    }
    if state.open.contains(path) {
        return Some(path.clone());
    }
    let Some(ignore) = ignore.filter(|i| i.matched_path_or_any_parents(path, false).is_ignore()) else {
        return Some(path.clone());
    };
    match kind_of(change) {
        (FileChange::Changed, _) => None,
        (FileChange::Deleted, _) => Some(path.clone()),
        (FileChange::Created, is_dir) => {
            let mut folders: Vec<&Path> = path.ancestors().take_while(|a| *a != root).collect();
            folders.reverse();
            let top = folders.into_iter().find(|a| ignore.matched(a, *a != path.as_path() || is_dir).is_ignore());
            Some(top.unwrap_or(path).to_path_buf())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_state(root: &Path) -> WatchState {
        WatchState {
            folders: vec![(root.to_path_buf(), project_ignore(root).map(Arc::new))],
            git_ignored: Arc::new([root.join("dist"), root.join("old.log")].into_iter().collect()),
            tree_read: [root.to_path_buf(), root.join("src")].into_iter().collect(),
            open: [root.join("src/a.rs")].into_iter().collect(),
            watchers: FileWatchers::default(),
        }
    }

    #[test]
    fn a_batch_is_sorted_out_by_what_it_affects() {
        let root = std::env::temp_dir().join(format!("kuana-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["src", "dist/out", ".git", "node_modules/x", "build/a"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "*.log\nbuild/\nnode_modules/\n").unwrap();
        for file in ["src/a.rs", "src/new.rs", "dist/out/x.js", "debug.log", "build/a/b.js", "build/a/c.js"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let state = watch_state(&root);
        let change = |rel: &str, structural| FsChange { path: root.join(rel), structural };
        let status_of = |c: &Classified| c.git.paths.iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();

        // Build output in a folder Git ignores: only Go to File hears of it
        // (it skips ignored folders itself).
        let c = classify(vec![change("dist/out/x.js", true), change("dist/out/y.js", true)], &state);
        assert!(c.git.is_empty() && c.tree.is_empty() && c.open.is_empty());
        assert_eq!(c.structural.len(), 2);
        // An open file written to: it's read again, and so is its status.
        let c = classify(vec![change("src/a.rs", false)], &state);
        assert!(c.tree.is_empty() && c.structural.is_empty());
        assert!(c.open.contains(&root.join("src/a.rs")));
        assert_eq!(status_of(&c), ["src/a.rs"]);
        // A new file where the tree has read: the tree hears of it.
        let c = classify(vec![change("src/new.rs", true)], &state);
        assert_eq!(c.tree, [root.join("src/new.rs")]);
        assert_eq!(status_of(&c), ["src/new.rs"]);
        // A log the project's rules ignore, written to: no status. A new
        // one gets one (the tree dims it once Git says it's ignored), and
        // so does one Git knew that went.
        assert!(classify(vec![change("debug.log", false)], &state).git.is_empty());
        assert_eq!(status_of(&classify(vec![change("debug.log", true)], &state)), ["debug.log"]);
        assert_eq!(status_of(&classify(vec![change("old.log", true)], &state)), ["old.log"]);
        let log_open = WatchState { open: [root.join("debug.log")].into_iter().collect(), ..watch_state(&root) };
        assert_eq!(status_of(&classify(vec![change("debug.log", false)], &log_open)), ["debug.log"]);
        // A new build folder the rules ignore: its status, not its files'.
        let c = classify(vec![change("build", true), change("build/a/b.js", true), change("build/a/c.js", true)], &state);
        assert_eq!(status_of(&c), ["build"]);
        // Dependencies the rules ignore: the tree only (where it has read).
        let c = classify(vec![change("node_modules/x/i.js", true)], &state);
        assert!(c.git.is_empty() && c.tree.is_empty() && c.structural.is_empty());
        // Build output Git ignores, the same; but a tab showing a file there reads it again.
        let target_ignored = WatchState { git_ignored: Arc::new([root.join("target")].into_iter().collect()), ..watch_state(&root) };
        let c = classify(vec![change("target/debug/out.rs", false)], &target_ignored);
        assert!(c.git.is_empty() && c.open.is_empty() && c.structural.is_empty());
        let open_output = WatchState { open: [root.join("target/debug/out.rs")].into_iter().collect(), ..target_ignored };
        assert!(classify(vec![change("target/debug/out.rs", false)], &open_output).open.contains(&root.join("target/debug/out.rs")));
        // A `target` folder nothing ignores (a Java package) is the project's.
        let c = classify(vec![change("src/target/T.java", true)], &state);
        assert_eq!(c.structural, [root.join("src/target/T.java")]);
        assert_eq!(status_of(&c), ["src/target/T.java"]);
        // Git's own: HEAD, the index, refs, all of the status; not objects.
        assert!(classify(vec![change(".git/index", false), change("src/a.rs", false)], &state).git.all);
        assert!(classify(vec![change(".git/objects/ab/cdef", true)], &state).git.is_empty());
        // New ignore rules: read again, and the status of their folder.
        let c = classify(vec![change(".gitignore", false)], &state);
        assert_eq!(c.ignores.iter().map(|(f, _)| f).collect::<Vec<_>>(), [&root]);
        assert_eq!(c.git.paths.iter().collect::<Vec<_>>(), [&root]);
        // A .gitignore further down isn't the folder's own rules.
        assert!(classify(vec![change("src/.gitignore", false)], &state).ignores.is_empty());
        // The watcher lost track: everything.
        assert!(classify(vec![FsChange { path: root.clone(), structural: true }], &state).everything);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Each change goes by the folder it's in: that folder's ignore rules,
    /// and none for paths outside every folder.
    #[test]
    fn several_folders_are_sorted_out_by_their_own_rules() {
        let base = std::env::temp_dir().join(format!("kuana-classify-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (api, web) = (base.join("api"), base.join("web"));
        for dir in [api.join("src"), web.join("src"), web.join("out")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(web.join(".gitignore"), "out/\n").unwrap();
        std::fs::write(web.join("out/bundle.js"), "").unwrap();
        let state = WatchState {
            folders: vec![(api.clone(), project_ignore(&api).map(Arc::new)), (web.clone(), project_ignore(&web).map(Arc::new))],
            git_ignored: Arc::default(),
            tree_read: HashSet::new(),
            open: HashSet::new(),
            watchers: FileWatchers::default(),
        };
        let change = |path: PathBuf| FsChange { path, structural: true };
        let c = classify(vec![change(api.join("src/a.rs")), change(web.join("out/bundle.js")), change(base.join("notes.txt"))], &state);
        assert_eq!(c.files, [api.join("src/a.rs"), web.join("out/bundle.js")]);
        // web's rules ignore its output: its status is read as the folder, not the file.
        assert_eq!(c.git.paths.iter().cloned().collect::<HashSet<_>>(), [api.join("src/a.rs"), web.join("out")].into_iter().collect());
        // Either folder itself going: everything.
        assert!(classify(vec![change(web.clone())], &state).everything);
        // web's .gitignore: its rules only.
        let c = classify(vec![FsChange { path: web.join(".gitignore"), structural: false }], &state);
        assert_eq!(c.ignores.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(), std::slice::from_ref(&web));
        std::fs::remove_dir_all(&base).ok();
    }
}
