//! Multibuffer tabs (`crate::multibuffer`): made from places (a Find panel
//! tab's, Find in Files' matches, the project's problems, which stay up to
//! date), their excerpts' editors heard like tabs' editors are, and tidied
//! up when the tab closes. Excerpt editors aren't in `Workspace::editors`
//! (that's the files' tabs: `tab_for` finds a file's tab there), so what's
//! per document is handed over here: a file without a tab gets its
//! breakpoints and bookmarks from the closed files' lists and writes them
//! back as they change, edits keep its document alive until saved (the
//! status bar lists it), and its language server hears it as open while an
//! excerpt shows it.

use std::{collections::BTreeMap, ops::Range};

use super::*;
use crate::{
    multibuffer::{FilePlaces, Marks, MultiBufferEvent, MultiBufferView},
    results_panel::ResultTarget,
    search::{ProjectMatch, SearchQuery},
};

/// Works out a file's places from its document.
type PlacesOf = Box<dyn FnOnce(&Document) -> FilePlaces>;

/// How often an open Problems tab checks whether what it lists changed.
const LIVE_PROBLEMS_CHECK: std::time::Duration = std::time::Duration::from_millis(500);

/// The Problems in Editor tab, kept up to date while it's open.
pub(super) struct LiveProblems {
    view: gpui::WeakEntity<MultiBufferView>,
    /// What it was last refreshed for (`problems_signature`).
    seen: Option<u64>,
    _task: Task<()>,
}

impl Workspace {
    /// A tab of excerpts around `targets`, by file in path order.
    pub(super) fn open_multibuffer(&mut self, title: SharedString, targets: Vec<ResultTarget>, marks: Marks, window: &mut Window, cx: &mut Context<Self>) {
        let mut files: BTreeMap<PathBuf, Vec<ResultTarget>> = BTreeMap::new();
        for target in targets {
            files.entry(target.path.clone()).or_default().push(target);
        }
        let files = files
            .into_iter()
            .map(|(path, targets)| {
                let places: PlacesOf = Box::new(move |doc: &Document| {
                    let mut places = places_in(doc, &targets);
                    if marks == Marks::None {
                        places.marks.clear();
                    }
                    places
                });
                (path, places)
            })
            .collect();
        self.open_places(title, files, window, cx);
    }

    /// Find in Files' results as a multibuffer, each match marked. The
    /// search read open files' unsaved text, so its offsets are the
    /// documents'; a file whose text changed since is searched again.
    pub(super) fn open_search_results(&mut self, title: SharedString, query: SearchQuery, matches: &[ProjectMatch], window: &mut Window, cx: &mut Context<Self>) {
        let mut files: BTreeMap<PathBuf, (Arc<str>, Vec<Range<usize>>)> = BTreeMap::new();
        for m in matches {
            files.entry(m.path.clone()).or_insert_with(|| (m.contents.clone(), Vec::new())).1.push(m.range.clone());
        }
        let files = files
            .into_iter()
            .map(|(path, (searched, ranges))| {
                let query = query.clone();
                let places: PlacesOf = Box::new(move |doc: &Document| match_places(doc, &searched, ranges, &query));
                (path, places)
            })
            .collect();
        self.open_places(title, files, window, cx);
    }

    /// A tab of excerpts, a file's places worked out once its document is read.
    fn open_places(&mut self, title: SharedString, files: Vec<(PathBuf, PlacesOf)>, window: &mut Window, cx: &mut Context<Self>) {
        if files.is_empty() {
            return;
        }
        let view = cx.new(|cx| MultiBufferView::new(title, self.root.clone(), files.len(), cx));
        self.add_tab(PaneItem::Excerpts(view.clone()), window, cx);
        for (path, places_of) in files {
            // A restored tab not read yet: read now, its document is the file's.
            if let Some(tab) = self.tab_for(&path, cx).filter(|t| !t.read(cx).is_loaded()) {
                self.load_tab(&tab, cx);
            }
            let load = self.documents.update(cx, |s, cx| s.open(&path, cx));
            let view = view.downgrade();
            cx.spawn(async move |_, cx| {
                let loaded = load.await;
                cx.update(|cx| {
                    let Some(view) = view.upgrade() else { return };
                    match loaded {
                        Ok(doc) if !doc.read(cx).is_stand_in() => {
                            let places = places_of(doc.read(cx));
                            view.update(cx, |v, cx| v.add_file(path, doc, places, cx));
                        }
                        _ => view.update(cx, |v, cx| v.file_failed(cx)),
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Open Results in Editor: the Find panel's current tab as a multibuffer.
    pub(super) fn open_results_in_editor(&mut self, _: &OpenResultsInEditor, window: &mut Window, cx: &mut Context<Self>) {
        match self.results.read(cx).current_places() {
            Some((title, targets)) => self.open_multibuffer(title, targets, Marks::Words, window, cx),
            None => self.toast(ToastKind::Info, "The Find panel lists no places to open", None, cx),
        }
    }

    /// Problems in Editor: the errors and warnings of the project, as
    /// excerpts, kept up to date (`check_live_problems`). Again while it's
    /// open: its tab.
    pub(super) fn problems_in_editor(&mut self, _: &ProblemsInEditor, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.live_problems.as_ref().and_then(|l| l.view.upgrade()) {
            self.activate_item(&PaneItem::Excerpts(view), window, cx);
            return;
        }
        let files = self.problem_files(cx);
        if files.is_empty() {
            self.toast(ToastKind::Success, "No problems", Some("None in open files, the language servers' reports or the last Run".into()), cx);
            return;
        }
        let view = cx.new(|cx| MultiBufferView::new("Problems", self.root.clone(), files.len(), cx));
        self.add_tab(PaneItem::Excerpts(view.clone()), window, cx);
        let task = cx.spawn_in(window, async move |ws, cx| {
            loop {
                cx.background_executor().timer(LIVE_PROBLEMS_CHECK).await;
                match ws.update_in(cx, |ws, window, cx| ws.check_live_problems(window, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        });
        self.live_problems = Some(LiveProblems { view: view.downgrade(), seen: None, _task: task });
        self.update_live_problems(files, false, cx);
    }

    /// Whether the Problems tab is still open; refreshes it when what it
    /// lists may have changed: a document's problems, a server's report, the
    /// Run's, or the keyboard leaving it (the excerpt it held may go now).
    fn check_live_problems(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.live_problems.as_ref().and_then(|l| l.view.upgrade()) else {
            self.live_problems = None;
            return false;
        };
        let hold = view.read(cx).contains_focus(window, cx);
        let signature = self.problems_signature(hold, cx);
        if self.live_problems.as_ref().is_some_and(|l| l.seen == Some(signature)) {
            return true;
        }
        if let Some(live) = self.live_problems.as_mut() {
            live.seen = Some(signature);
        }
        let mut files = self.problem_files(cx);
        // Files shown that have none now: their excerpts go (unless held).
        for path in view.read(cx).paths() {
            files.entry(path).or_default();
        }
        self.update_live_problems(files, hold, cx);
        true
    }

    /// Gives the Problems tab each file's places, as its document has them.
    fn update_live_problems(&mut self, files: BTreeMap<PathBuf, Vec<ResultTarget>>, hold: bool, cx: &mut Context<Self>) {
        let Some(view) = self.live_problems.as_ref().and_then(|l| l.view.upgrade()) else { return };
        for (path, targets) in files {
            if let Some(tab) = self.tab_for(&path, cx).filter(|t| !t.read(cx).is_loaded()) {
                self.load_tab(&tab, cx);
            }
            let load = self.documents.update(cx, |s, cx| s.open(&path, cx));
            let view = view.downgrade();
            cx.spawn(async move |_, cx| {
                let loaded = load.await;
                cx.update(|cx| {
                    let Some(view) = view.upgrade() else { return };
                    match loaded {
                        Ok(doc) if !doc.read(cx).is_stand_in() => {
                            let mut places = places_in(doc.read(cx), &targets);
                            places.marks.clear();
                            view.update(cx, |v, cx| v.set_file_places(path, doc, places, hold, cx));
                        }
                        _ => view.update(cx, |v, cx| v.file_failed(cx)),
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// The project's errors and warnings by file: open documents' (tabs,
    /// excerpts, files edited without a tab), the language servers' reports
    /// for files not open, and the last Run's.
    fn problem_files(&self, cx: &App) -> BTreeMap<PathBuf, Vec<ResultTarget>> {
        let mut files: BTreeMap<PathBuf, Vec<ResultTarget>> = BTreeMap::new();
        let mut open = HashSet::new();
        for doc in self.open_documents(cx) {
            let d = doc.read(cx);
            let Some(path) = d.path.clone().filter(|p| open.insert(p.clone())) else { continue };
            for problem in d.diagnostics.iter().filter(|p| p.severity.is_problem()) {
                let row = d.row_of(problem.range.start);
                let character = (problem.range.start - d.line_start(row)) as u32;
                files.entry(path.clone()).or_default().push(ResultTarget {
                    path: path.clone(),
                    position: Position { line: row as u32, character },
                    encoding: Encoding::Utf8,
                });
            }
        }
        for (path, position, encoding) in self.lsp.read(cx).problem_places(&open) {
            files.entry(path.clone()).or_default().push(ResultTarget { path, position, encoding });
        }
        // The Run's, unless the file shows the same line already.
        for p in self.problems.read(cx).run_problems() {
            let line = p.line.saturating_sub(1);
            let targets = files.entry(p.path.clone()).or_default();
            if !targets.iter().any(|t| t.position.line == line) {
                targets.push(ResultTarget { path: p.path.clone(), position: Position { line, character: 0 }, encoding: Encoding::Utf8 });
            }
        }
        files.retain(|_, targets| !targets.is_empty());
        files
    }

    /// What the Problems tab's contents depend on, as one number.
    fn problems_signature(&self, hold: bool, cx: &App) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hold.hash(&mut hasher);
        self.lsp.read(cx).diagnostics_generation().hash(&mut hasher);
        self.problems.read(cx).run_problems().len().hash(&mut hasher);
        for doc in self.open_documents(cx) {
            let d = doc.read(cx);
            (d.id(), d.diagnostics_generation()).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Every document open in this window: tabs', excerpts', and files
    /// edited without a tab.
    fn open_documents(&self, cx: &App) -> Vec<Entity<Document>> {
        let mut docs: Vec<Entity<Document>> = self.editors.iter().filter(|e| e.read(cx).is_loaded()).map(|e| e.read(cx).doc.clone()).collect();
        docs.extend(self.excerpt_editors(cx).iter().map(|e| e.read(cx).doc.clone()));
        docs.extend(self.documents.read(cx).kept(cx));
        docs
    }

    /// A new multibuffer tab: the workspace hears its excerpts and what it asks for.
    pub(super) fn add_multibuffer(&mut self, view: &Entity<MultiBufferView>, window: &mut Window, cx: &mut Context<Self>) {
        let subscription = cx.subscribe_in(view, window, |ws, _, event: &MultiBufferEvent, window, cx| match event {
            MultiBufferEvent::Added(editors) => {
                for editor in editors {
                    ws.wire_excerpt(editor, window, cx);
                }
            }
            MultiBufferEvent::Removed(editors) => {
                for editor in editors {
                    ws.unwire_excerpt(editor, cx);
                }
            }
            MultiBufferEvent::Open { path, offset } => {
                let offset = *offset;
                ws.open_path(path.clone(), window, cx);
                if let Some(editor) = ws.tab_for(path, cx) {
                    editor.update(cx, |e, cx| e.go_to(offset, window, cx));
                    window.focus(&editor.focus_handle(cx));
                }
            }
            MultiBufferEvent::Changed => cx.notify(),
        });
        // Dropped with the tab (see `remove_item`).
        self.tab_subscriptions.insert(view.entity_id(), vec![subscription]);
        // A split of one comes with its excerpts made.
        for editor in view.read(cx).editors().cloned().collect::<Vec<_>>() {
            self.wire_excerpt(&editor, window, cx);
        }
    }

    /// Closes a multibuffer: its files' unsaved edits stay (kept without a tab).
    pub(super) fn close_multibuffer(&mut self, view: &Entity<MultiBufferView>, window: &mut Window, cx: &mut Context<Self>) {
        let editors: Vec<Entity<Editor>> = view.read(cx).editors().cloned().collect();
        self.remove_item(&PaneItem::Excerpts(view.clone()), window, cx);
        for editor in &editors {
            self.unwire_excerpt(editor, cx);
        }
    }

    /// Hears an excerpt's editor as a tab's: its menus, language server
    /// requests and marks; and starts what its file needs.
    fn wire_excerpt(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let subscriptions = vec![
            cx.subscribe(editor, |ws, editor, _: &EditorEvent, cx| {
                // A file without a tab stays (unsaved) when the multibuffer closes.
                let doc = editor.read(cx).doc.clone();
                if !ws.has_tab(&doc, cx) {
                    ws.documents.update(cx, |s, cx| s.keep(&doc, cx));
                }
                ws.schedule_auto_save(&editor, cx);
                ws.todo_edited(&editor, cx);
            }),
            cx.subscribe_in(editor, window, |ws, editor, event: &EditorContextMenu, window, cx| {
                ws.open_editor_menu(editor.clone(), event.0, window, cx);
            }),
            cx.subscribe_in(editor, window, |ws, _, event: &EditorLspEvent, window, cx| {
                ws.on_editor_lsp_event(event, window, cx);
            }),
            cx.subscribe_in(editor, window, |ws, _, event: &EditorRunEvent, window, cx| {
                ws.on_run_event(event, window, cx);
            }),
            cx.subscribe(editor, |ws, editor, _: &EditorBreakpointsChanged, cx| {
                ws.excerpt_marks_changed(&editor, cx);
                ws.breakpoints_changed(&editor, cx);
            }),
            cx.subscribe(editor, |ws, editor, _: &EditorBookmarksChanged, cx| {
                ws.excerpt_marks_changed(&editor, cx);
                ws.bookmarks_changed(cx);
            }),
            cx.subscribe_in(editor, window, |ws, editor, event: &EditorBreakpointMenu, window, cx| {
                ws.open_breakpoint_menu(editor.clone(), event.row, event.position, window, cx);
            }),
            cx.subscribe_in(editor, window, |ws, _, event: &EditorGitEvent, window, cx| {
                ws.run_git(event.0.clone(), window, cx);
            }),
        ];
        self.tab_subscriptions.insert(editor.entity_id(), subscriptions);
        let doc = editor.read(cx).doc.clone();
        self.documents.update(cx, |s, cx| s.add(&doc, cx));
        // A file without a tab: its marks from the closed files' lists.
        if !self.has_tab(&doc, cx)
            && let Some(path) = doc.read(cx).path.clone()
        {
            let (breakpoints, bookmarks) = (self.breakpoints.get(&path).cloned(), self.bookmarks.get(&path).cloned());
            editor.update(cx, |e, cx| {
                if let Some(b) = breakpoints {
                    e.set_breakpoints(&b, cx);
                }
                if let Some(b) = bookmarks {
                    e.set_bookmarks(&b, cx);
                }
            });
        }
        let diff = self.documents.update(cx, |s, cx| s.diff_for(&doc, cx));
        let statuses = self.test_statuses.clone();
        editor.update(cx, |e, cx| {
            e.set_git_diff(diff, cx);
            e.set_test_statuses(statuses, cx);
        });
        self.lsp.update(cx, |s, cx| s.register(editor, cx));
    }

    /// Stops hearing an excerpt's editor; a file nothing shows any more
    /// (and without unsaved edits) is closed for its language server.
    fn unwire_excerpt(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.tab_subscriptions.remove(&editor.entity_id());
        self.auto_save_tasks.remove(&editor.entity_id());
        let doc = editor.read(cx).doc.clone();
        let shown = self.has_tab(&doc, cx) || self.excerpt_editors(cx).iter().any(|e| e != editor && e.read(cx).doc == doc);
        if !shown
            && !doc.read(cx).is_dirty()
            && let Some(path) = doc.read(cx).path.clone()
        {
            self.lsp.update(cx, |s, _| s.did_close(&path));
        }
    }

    /// A file without a tab: its breakpoints and bookmarks go to the closed
    /// files' lists (the session and the debugger read them there).
    fn excerpt_marks_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let doc = editor.read(cx).doc.clone();
        if self.has_tab(&doc, cx) {
            return;
        }
        let Some(path) = doc.read(cx).path.clone() else { return };
        let (breakpoints, bookmarks) = (doc.read(cx).breakpoints(), doc.read(cx).bookmarks());
        if breakpoints.is_empty() {
            self.breakpoints.remove(&path);
        } else {
            self.breakpoints.insert(path.clone(), breakpoints);
        }
        if bookmarks.is_empty() {
            self.bookmarks.remove(&path);
        } else {
            self.bookmarks.insert(path, bookmarks);
        }
    }

    /// Whether a file's tab (read) shows `doc`.
    fn has_tab(&self, doc: &Entity<Document>, cx: &App) -> bool {
        self.editors.iter().any(|e| e.read(cx).is_loaded() && e.read(cx).doc == *doc)
    }

    /// Every editor showing a file: the tabs', then the multibuffers'
    /// excerpts'. For what reaches every shown editor (settings, trust,
    /// languages, test results, reloads); `editors` is the files' tabs only.
    pub(super) fn shown_editors(&self, cx: &App) -> Vec<Entity<Editor>> {
        let mut all = self.editors.clone();
        all.extend(self.excerpt_editors(cx));
        all
    }

    /// Every multibuffer's excerpt editors.
    pub(super) fn excerpt_editors(&self, cx: &App) -> Vec<Entity<Editor>> {
        self.items(cx).iter().filter_map(PaneItem::excerpts).flat_map(|v| v.read(cx).editors().cloned().collect::<Vec<_>>()).collect()
    }

    /// Save in a multibuffer: every file of it with unsaved edits.
    pub(super) fn save_multibuffer(&mut self, view: &Entity<MultiBufferView>, window: &mut Window, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let dirty: Vec<Entity<Editor>> = view
            .read(cx)
            .editors()
            .filter(|e| {
                let doc = e.read(cx).doc.read(cx);
                doc.is_dirty() && seen.insert(doc.id())
            })
            .cloned()
            .collect();
        for editor in dirty {
            self.save_one(editor, window, cx);
        }
    }
}

/// What a file's targets show: their lines, and the word at each to mark.
fn places_in(doc: &Document, targets: &[ResultTarget]) -> FilePlaces {
    let rope = doc.rope();
    let mut places = FilePlaces::default();
    for target in targets {
        let offset = protocol::position_to_offset(rope, target.position, target.encoding).min(doc.len());
        let row = doc.row_of(offset);
        places.rows.push(row);
        let line = doc.line_text(row);
        let col = offset - doc.line_start(row);
        let word_end = line[col.min(line.len())..].find(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(line.len(), |n| col + n);
        if word_end > col {
            places.marks.push(offset..doc.line_start(row) + word_end);
        }
    }
    places
}

/// A file's search matches as places: the ranges found when the document
/// still has the text that was `searched`, else the query's matches in its
/// text now.
fn match_places(doc: &Document, searched: &str, ranges: Vec<Range<usize>>, query: &SearchQuery) -> FilePlaces {
    let ranges = if *doc.rope() == *searched {
        ranges
    } else {
        query.find_all(&doc.rope().to_string())
    };
    FilePlaces { rows: ranges.iter().map(|r| doc.row_of(r.start)).collect(), marks: ranges }
}
