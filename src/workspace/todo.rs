//! The TODO list (JetBrains' TODO tool window): the project's TODO items
//! (`crate::todo`) as a tab of the Find panel, by file, for the whole
//! project, the current file or the files with changes. The project is read
//! once, the first time the list is asked for; after that only files that
//! change (on disk, or edited in a tab) are read again, and the tab follows.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

use super::*;
use crate::{
    results_panel::{Leading, ResultNode, ResultTarget, TabButton},
    todo::{self, Todo},
};

/// Finds the TODO tab in the Find panel again.
const TODO_TAB: &str = "todo";
/// How long after a change (typing, a save, a checkout) files are read again.
const RESCAN_DELAY: Duration = Duration::from_millis(400);

/// Which files the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TodoScope {
    #[default]
    Project,
    CurrentFile,
    ChangedFiles,
}

#[derive(Default)]
pub(super) struct TodoList {
    /// Each file's items (files without any left out), once the first read
    /// of the project is done; None until the list is first asked for.
    files: Option<BTreeMap<PathBuf, Arc<Vec<Todo>>>>,
    /// The patterns the items were found with.
    patterns: Vec<String>,
    scope: TodoScope,
    /// The whole project is being read.
    scanning: bool,
    /// Asked for before the project's files were listed: read them once they are.
    waiting_for_files: bool,
    /// Files (or folders) to read again.
    pending: HashSet<PathBuf>,
    /// The pause before reading the pending files.
    timer: Option<Task<()>>,
    /// Reading files (the project, or the pending ones).
    task: Option<Task<()>>,
    cancel: Arc<AtomicBool>,
}

impl TodoList {
    /// Kept up to date (the list was asked for in this project).
    fn active(&self) -> bool {
        self.files.is_some() || self.scanning
    }
}

impl Workspace {
    /// Show TODO: the list in the Find panel, reading the project the first time.
    pub(super) fn show_todo(&mut self, _: &ShowTodo, window: &mut Window, cx: &mut Context<Self>) {
        if !self.todo.active() {
            self.scan_todos(cx);
        }
        let (title, nodes, buttons) = self.todo_tab(cx);
        self.results.update(cx, |p, cx| p.show_kind(TODO_TAB, title, nodes, buttons, cx));
        self.has_results = true;
        self.bottom = Some(BottomPanel::Results);
        window.focus(&self.results.focus_handle(cx));
        cx.notify();
    }

    /// Another project: the list starts over when it's asked for.
    pub(super) fn reset_todos(&mut self) {
        self.todo.cancel.store(true, Ordering::Relaxed);
        self.todo = TodoList::default();
    }

    /// Reads every file of the project, in the background.
    fn scan_todos(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        self.todo.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.todo.cancel = cancel.clone();
        self.todo.patterns = Settings::get(cx).todo_patterns();
        self.todo.pending.clear();
        let Some(regex) = todo::regex(&self.todo.patterns) else {
            self.todo.files = Some(BTreeMap::new());
            self.todo.scanning = false;
            self.todo.task = None;
            self.refresh_todo_tab(cx);
            return;
        };
        let paths = self.project.searchable();
        // The project's files aren't listed yet: read them when they are.
        self.todo.waiting_for_files = paths.is_empty() && self.project.root() != root.as_path();
        let overrides = self.open_buffers(cx);
        self.todo.scanning = true;
        self.todo.task = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { todo::scan(&paths, &overrides, &regex, &cancel) }).await;
            this.update(cx, |ws, cx| {
                ws.todo.scanning = false;
                ws.todo.files = Some(found.into_iter().filter(|(_, t)| !t.is_empty()).map(|(p, t)| (p, Arc::new(t))).collect());
                ws.todo.task = None;
                ws.refresh_todo_tab(cx);
                // Changes that came while it read.
                if !ws.todo.pending.is_empty() {
                    ws.schedule_todo_rescan(cx);
                }
            })
            .ok();
        }));
    }

    /// The project's files were listed (or listed again).
    pub(super) fn todo_project_listed(&mut self, cx: &mut Context<Self>) {
        if self.todo.waiting_for_files {
            self.scan_todos(cx);
        }
    }

    /// Files (or folders) changed on disk or in a tab: read them again soon.
    pub(super) fn todo_files_changed(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        if !self.todo.active() {
            return;
        }
        let Some(root) = self.root.clone() else { return };
        self.todo.pending.extend(paths.into_iter().filter(|p| p.starts_with(&root)));
        if !self.todo.scanning && !self.todo.pending.is_empty() {
            self.schedule_todo_rescan(cx);
        }
    }

    /// A tab's text changed: its file's items follow its unsaved text.
    pub(super) fn todo_edited(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if !self.todo.active() {
            return;
        }
        if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
            self.todo_files_changed([path], cx);
        }
    }

    /// Settings changed: other patterns mean reading everything again.
    pub(super) fn todo_settings_changed(&mut self, cx: &mut Context<Self>) {
        if self.todo.active() && Settings::get(cx).todo_patterns() != self.todo.patterns {
            self.scan_todos(cx);
        }
    }

    /// Reads the pending files again after a pause (one more change restarts it).
    fn schedule_todo_rescan(&mut self, cx: &mut Context<Self>) {
        self.todo.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESCAN_DELAY).await;
            this.update(cx, |ws, cx| ws.rescan_todos(cx)).ok();
        }));
    }

    fn rescan_todos(&mut self, cx: &mut Context<Self>) {
        self.todo.timer = None;
        // A read is underway: it asks again for what's pending when it's done.
        if self.todo.task.is_some() {
            return;
        }
        let Some(regex) = todo::regex(&self.todo.patterns) else { return };
        let pending = std::mem::take(&mut self.todo.pending);
        let Some(files) = self.todo.files.as_mut() else { return };
        // A folder that went (or came, renamed): what was listed under it
        // goes, and what the project has in it now is read.
        let mut wanted = Vec::new();
        let openable = self.project.openable();
        for path in pending {
            files.retain(|p, _| p == &path || !p.starts_with(&path));
            if path.is_dir() {
                wanted.extend(openable.iter().filter(|p| p.starts_with(&path)).cloned());
            } else {
                wanted.push(path);
            }
        }
        // Not files Find in Files skips (ignored ones), nor ones that went.
        let (project, ignore) = (self.project.clone(), self.ignore.clone());
        wanted.retain(|path| {
            let listed = project.is_searchable(path);
            let unlisted = !project.lists(path) && path.is_file() && !ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore());
            if !(listed || unlisted) {
                files.remove(path);
            }
            listed || unlisted
        });
        if wanted.is_empty() {
            self.refresh_todo_tab(cx);
            return;
        }
        let overrides = self.unsaved_texts(&wanted, cx);
        let cancel = self.todo.cancel.clone();
        self.todo.task = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { todo::scan(&wanted, &overrides, &regex, &cancel) }).await;
            this.update(cx, |ws, cx| {
                ws.todo.task = None;
                let Some(files) = ws.todo.files.as_mut() else { return };
                for (path, todos) in found {
                    if todos.is_empty() {
                        files.remove(&path);
                    } else {
                        files.insert(path, Arc::new(todos));
                    }
                }
                ws.refresh_todo_tab(cx);
                if !ws.todo.pending.is_empty() {
                    ws.schedule_todo_rescan(cx);
                }
            })
            .ok();
        }));
    }

    /// The unsaved text of the open files among `paths` (the rest is read from disk).
    fn unsaved_texts(&self, paths: &[PathBuf], cx: &App) -> HashMap<PathBuf, Arc<str>> {
        let wanted: HashSet<&PathBuf> = paths.iter().collect();
        let mut texts = HashMap::new();
        for editor in &self.editors {
            let e = editor.read(cx);
            let doc = e.doc.read(cx);
            let Some(path) = doc.path.clone().filter(|p| wanted.contains(p)) else { continue };
            let text = match e.unloaded() {
                Some(unloaded) => unloaded.unsaved.as_ref().map(|r| r.to_string()),
                None => doc.is_dirty().then(|| doc.rope().to_string()),
            };
            if let Some(text) = text {
                texts.insert(path, Arc::from(text));
            }
        }
        texts
    }

    /// What the list shows depends on the current file or the changed ones:
    /// one of those changed.
    pub(super) fn todo_scope_input_changed(&mut self, scope: TodoScope, cx: &mut Context<Self>) {
        if self.todo.scope == scope {
            self.refresh_todo_tab(cx);
        }
    }

    fn set_todo_scope(&mut self, scope: TodoScope, cx: &mut Context<Self>) {
        self.todo.scope = scope;
        self.refresh_todo_tab(cx);
    }

    /// Updates the TODO tab, if it's open.
    fn refresh_todo_tab(&mut self, cx: &mut Context<Self>) {
        if !self.todo.active() {
            return;
        }
        let (title, nodes, buttons) = self.todo_tab(cx);
        self.results.update(cx, |p, cx| p.update_kind(TODO_TAB, title, nodes, buttons, cx));
    }

    /// The tab's title, tree and scope buttons.
    fn todo_tab(&self, cx: &mut Context<Self>) -> (SharedString, Vec<ResultNode>, Vec<TabButton>) {
        let scope = self.todo.scope;
        let shown: Option<HashSet<PathBuf>> = match scope {
            TodoScope::Project => None,
            TodoScope::CurrentFile => Some(self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()).into_iter().collect()),
            TodoScope::ChangedFiles => Some(self.git_status(cx).files.iter().map(|f| f.path.clone()).collect()),
        };
        let empty = BTreeMap::new();
        let files = self.todo.files.as_ref().unwrap_or(&empty);
        let mut nodes: Vec<ResultNode> = files
            .iter()
            .filter(|(path, _)| shown.as_ref().is_none_or(|s| s.contains(*path)))
            .map(|(path, todos)| todo_file_node(path, todos, &self.location_label(path)))
            .collect();
        if nodes.is_empty() {
            nodes.push(ResultNode::message(match (self.todo.scanning, scope) {
                (true, _) => "Looking for TODO items…",
                (false, TodoScope::Project) => "No TODO items in the project",
                (false, TodoScope::CurrentFile) => "No TODO items in the current file",
                (false, TodoScope::ChangedFiles) => "No TODO items in changed files",
            }));
        }
        let this = cx.entity().downgrade();
        let button = |label: &str, icon: Icon, to: TodoScope| {
            let this = this.clone();
            TabButton {
                icon,
                label: label.to_string().into(),
                on: scope == to,
                on_click: std::rc::Rc::new(move |_, cx| {
                    this.update(cx, |ws, cx| ws.set_todo_scope(to, cx)).ok();
                }),
            }
        };
        let buttons = vec![
            button("Project", Icon::Folder, TodoScope::Project),
            button("Current File", Icon::File, TodoScope::CurrentFile),
            button("Changed Files", Icon::GitCompare, TodoScope::ChangedFiles),
        ];
        ("TODO".into(), nodes, buttons)
    }
}

/// A file's group: its items, each going to its word.
fn todo_file_node(path: &Path, todos: &[Todo], shown: &str) -> ResultNode {
    let children = todos
        .iter()
        .map(|t| ResultNode {
            leading: Leading::Line(t.line as u32 + 1),
            label: t.text.clone().into(),
            highlight: Some(0..t.keyword.min(t.text.len())),
            detail: None,
            target: Some(ResultTarget {
                path: path.to_path_buf(),
                position: Position { line: t.line as u32, character: t.column as u32 },
                encoding: Encoding::Utf8,
            }),
            open: None,
            children: Vec::new(),
            loader: None,
            expanded: false,
        })
        .collect::<Vec<_>>();
    let (folder, name) = match shown.rsplit_once('/') {
        Some((folder, name)) => (Some(folder.to_string()), name.to_string()),
        None => (None, shown.to_string()),
    };
    let count = format!("{} item{}", todos.len(), if todos.len() == 1 { "" } else { "s" });
    ResultNode {
        leading: Leading::File(path.to_path_buf()),
        label: name.into(),
        highlight: None,
        detail: Some(match folder {
            Some(folder) => format!("{folder}  ·  {count}").into(),
            None => count.into(),
        }),
        target: None,
        open: None,
        children,
        loader: None,
        expanded: true,
    }
}
