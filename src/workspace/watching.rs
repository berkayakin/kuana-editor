//! Watching the project folder: what a change on disk updates. Each batch
//! of changes is sorted out in the background (`classify`: it stats paths,
//! matches ignore rules and language servers' globs), so churn in build
//! output or a venv costs the UI thread a short summary, not a look at
//! every path.

use ignore::gitignore::Gitignore;

use super::{git_ops::StatusWork, *};
use crate::lsp::{FileWatchers, ServerKey};

impl Workspace {
    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project folder so changes made outside Kuana show up right away.
    pub(super) fn start_watching(&mut self, cx: &mut Context<Self>) {
        self.watcher = None;
        self.watch_task = None;
        let Some(root) = self.root.clone() else { return };
        self.ignore = project_ignore(&root).map(Arc::new);
        let (watcher, mut changes) = match Watcher::new(&root) {
            Ok(watching) => watching,
            Err(err) => {
                eprintln!("couldn't watch {}: {err}", root.display());
                return;
            }
        };
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

    /// What sorting out a batch of changes goes by, as it is now.
    fn watch_state(&self, cx: &App) -> Option<WatchState> {
        let root = self.root.clone()?;
        let mut open: HashSet<PathBuf> = self.editors.iter().filter_map(|e| e.read(cx).doc.read(cx).path.clone()).collect();
        open.extend(self.images(cx).iter().map(|i| i.read(cx).path().to_path_buf()));
        if let Some(Modal::Diff(view)) = &self.modals.current
            && let Some(path) = view.read(cx).working_tree_file()
        {
            open.insert(path.to_path_buf());
        }
        Some(WatchState {
            root,
            ignore: self.ignore.clone(),
            git_ignored: self.git_ignored.clone(),
            tree_read: self.tree.read(cx).read_folders(),
            open,
            watchers: self.lsp.read(cx).file_watchers(),
        })
    }

    /// Updates what a batch of changes on disk affects (`classify` sorted
    /// it out): the tree (in folders it has read), Go to File, Git status,
    /// language servers and open files.
    pub(super) fn disk_changed(&mut self, changed: Classified, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        if root.is_dir() == self.root_gone {
            self.root_gone = !self.root_gone;
            if self.root_gone {
                self.toast(ToastKind::Error, "The project folder was deleted", Some(clone_dialog::display_path(&root)), cx);
            }
            // Gone: the branch, changes and log go with it (no repository is
            // found there); back: they're read again.
            self.detect_repo(root, cx);
        }
        // A venv may have been created or removed.
        self.venv_shown.take();
        // The watcher lost events: everything may have changed.
        if changed.everything {
            self.tree.update(cx, |t, cx| t.relist_all(cx));
            self.refresh_file_index(cx);
            self.refresh_git(cx);
            self.reload_documents(|_| true, cx);
            cx.notify();
            return;
        }
        if let Some(ignore) = changed.ignore {
            self.ignore = ignore;
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
        if let Some(Modal::Diff(view)) = &self.modals.current
            && view.read(cx).working_tree_file().is_some_and(|p| paths.contains(p))
        {
            view.update(cx, |v, cx| v.reload(cx));
        }
    }

    /// Has the documents whose files `wanted` picks read them again (in the
    /// background), each through one of its tabs: splits share it.
    pub(super) fn reload_documents(&mut self, wanted: impl Fn(&Path) -> bool, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        for editor in self.editors.clone() {
            let doc = editor.read(cx).doc.read(cx);
            if editor.read(cx).is_loaded() && doc.path.as_deref().is_some_and(&wanted) && seen.insert(doc.id()) {
                editor.update(cx, |e, cx| e.reload_if_changed(cx));
            }
        }
    }

    /// Scans the project's files in the background (a new project, new
    /// ignore rules); changes waiting meanwhile are applied after.
    pub(super) fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.project_changes.clear();
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = cx.background_executor().spawn(async move { ProjectSnapshot::scan(&root) }).await;
            this.update(cx, |ws, cx| ws.project_updated(snapshot, cx)).ok();
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
        let base = self.project.clone();
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let (snapshot, applied) = cx
                .background_executor()
                .spawn(async move {
                    let mut snapshot = (*base).clone();
                    let applied = snapshot.apply(paths.iter().map(PathBuf::as_path));
                    (snapshot, applied)
                })
                .await;
            this.update(cx, |ws, cx| match applied {
                Applied::Rescan => ws.refresh_file_index(cx),
                Applied::Changed => ws.project_updated(snapshot, cx),
                Applied::Unchanged => ws.project_updated_unchanged(cx),
            })
            .ok();
        }));
    }

    fn project_updated(&mut self, snapshot: ProjectSnapshot, cx: &mut Context<Self>) {
        // Freeing a big project's paths takes a while: not on the UI thread.
        let old = std::mem::replace(&mut self.project, Arc::new(snapshot));
        cx.background_executor().spawn(async move { drop(old) }).detach();
        if let Some(Modal::Picker(picker)) = &self.modals.current {
            let files = self.project.openable();
            picker.update(cx, |p, cx| p.set_files(files, cx));
        }
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

/// What a batch of changes is sorted out by (`classify`), taken from the
/// workspace when it comes.
pub(super) struct WatchState {
    root: PathBuf,
    /// The project's own ignore rules.
    ignore: Option<Arc<Gitignore>>,
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
    /// The project's ignore rules, read again (a .gitignore changed).
    ignore: Option<Option<Arc<Gitignore>>>,
    /// Git status to read again: all of it (`.git` changed), or paths'.
    git: StatusWork,
    /// Open files that changed.
    open: HashSet<PathBuf>,
    /// Each language server's `didChangeWatchedFiles`, for what it watches.
    lsp: Vec<(ServerKey, serde_json::Value)>,
    /// A project settings file changed (a checkout, another editor).
    project_settings: bool,
}

/// Sorts out a batch of changes (blocking: in the background). Paths in a
/// `target` or `node_modules` folder that's ignored matter to the tree (and
/// open files) only.
pub(super) fn classify(changes: Vec<FsChange>, state: &WatchState) -> Classified {
    let root = &state.root;
    let mut out = Classified::default();
    if changes.iter().any(|c| &c.path == root) {
        out.everything = true;
        return out;
    }
    // New rules count for the whole batch.
    let mut ignore = state.ignore.clone();
    let in_project = |path: &Path, ignore: Option<&Gitignore>| path.starts_with(root) && !in_ignored_heavy_folder(path, state, ignore);
    if changes.iter().any(|c| {
        in_project(&c.path, ignore.as_deref()) && c.path.file_name().is_some_and(|n| n == ".gitignore") && watcher::git_path(&c.path) == GitPath::WorkTree
    }) {
        ignore = project_ignore(root).map(Arc::new);
        out.ignore = Some(ignore.clone());
    }
    // Each path's kind of change, found once, and only when needed.
    let mut found: HashMap<PathBuf, Option<bool>> = HashMap::new();
    let mut kind_of = |change: &FsChange| {
        let is_dir = *found.entry(change.path.clone()).or_insert_with(|| std::fs::metadata(&change.path).ok().map(|m| m.is_dir()));
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
                if !change.path.starts_with(root) {
                    continue;
                }
                // Generated code a tab shows (a build script's output) is read again.
                if state.open.contains(&change.path) {
                    out.open.insert(change.path.clone());
                }
                if !in_project(&change.path, ignore.as_deref()) {
                    continue;
                }
                if change.structural {
                    out.structural.push(change.path.clone());
                }
                if let Some(path) = status_path(&change, state, ignore.as_deref(), &mut kind_of) {
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

/// Whether `path` is in a `target` or `node_modules` folder that the
/// project's rules or Git ignore: build output and dependencies, whose churn
/// is skipped without a look at each path. One that isn't ignored (a Java
/// package named `target`) is the project's like any other folder.
fn in_ignored_heavy_folder(path: &Path, state: &WatchState, ignore: Option<&Gitignore>) -> bool {
    let Some(folder) = watcher::heavy_folder(&state.root, path) else { return false };
    folder.ancestors().take_while(|a| a.starts_with(&state.root)).any(|a| state.git_ignored.contains(a))
        || ignore.is_some_and(|i| i.matched_path_or_any_parents(&folder, true).is_ignore())
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
            let mut folders: Vec<&Path> = path.ancestors().take_while(|a| *a != state.root.as_path()).collect();
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
            root: root.to_path_buf(),
            ignore: project_ignore(root).map(Arc::new),
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
        assert!(c.ignore.is_some());
        assert_eq!(c.git.paths.iter().collect::<Vec<_>>(), [&root]);
        // The watcher lost track: everything.
        assert!(classify(vec![FsChange { path: root.clone(), structural: true }], &state).everything);
        std::fs::remove_dir_all(&root).ok();
    }
}
