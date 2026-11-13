//! Multibuffer tabs (`crate::multibuffer`): made from places (a Find panel
//! tab's), their excerpts' editors heard like tabs' editors are, and tidied
//! up when the tab closes. Excerpt editors aren't in `Workspace::editors`
//! (that's the files' tabs: `tab_for` finds a file's tab there), so what's
//! per document is handed over here: a file without a tab gets its
//! breakpoints and bookmarks from the closed files' lists and writes them
//! back as they change, edits keep its document alive until saved (the
//! status bar lists it), and its language server hears it as open while an
//! excerpt shows it.

use std::collections::BTreeMap;

use super::*;
use crate::{
    multibuffer::{FilePlaces, Marks, MultiBufferEvent, MultiBufferView},
    results_panel::ResultTarget,
};

impl Workspace {
    /// A tab of excerpts around `targets`, by file in path order.
    pub(super) fn open_multibuffer(&mut self, title: SharedString, targets: Vec<ResultTarget>, marks: Marks, window: &mut Window, cx: &mut Context<Self>) {
        let mut files: BTreeMap<PathBuf, Vec<ResultTarget>> = BTreeMap::new();
        for target in targets {
            files.entry(target.path.clone()).or_default().push(target);
        }
        if files.is_empty() {
            return;
        }
        let view = cx.new(|cx| MultiBufferView::new(title, self.root.clone(), files.len(), cx));
        self.add_tab(PaneItem::Excerpts(view.clone()), window, cx);
        for (path, targets) in files {
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
                            let mut places = places_in(doc.read(cx), &targets);
                            if marks == Marks::None {
                                places.marks.clear();
                            }
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

    /// Problems in Editor: the errors and warnings of every open file (tabs,
    /// excerpts, files edited without a tab) and the last Run's, as excerpts.
    pub(super) fn problems_in_editor(&mut self, _: &ProblemsInEditor, window: &mut Window, cx: &mut Context<Self>) {
        let mut docs: Vec<Entity<Document>> = self.editors.iter().map(|e| e.read(cx).doc.clone()).collect();
        docs.extend(self.excerpt_editors(cx).iter().map(|e| e.read(cx).doc.clone()));
        docs.extend(self.documents.read(cx).kept(cx));
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for doc in docs {
            let d = doc.read(cx);
            let Some(path) = d.path.clone().filter(|_| seen.insert(d.id())) else { continue };
            for problem in d.diagnostics.iter().filter(|p| p.severity.is_problem()) {
                let row = d.row_of(problem.range.start);
                let character = (problem.range.start - d.line_start(row)) as u32;
                targets.push(ResultTarget { path: path.clone(), position: Position { line: row as u32, character }, encoding: Encoding::Utf8 });
            }
        }
        // The Run's, unless an open file shows the same line already.
        for p in self.problems.read(cx).run_problems() {
            let target = ResultTarget {
                path: p.path.clone(),
                position: Position { line: p.line.saturating_sub(1), character: 0 },
                encoding: Encoding::Utf8,
            };
            if !targets.iter().any(|t| t.path == target.path && t.position.line == target.position.line) {
                targets.push(target);
            }
        }
        if targets.is_empty() {
            self.toast(ToastKind::Success, "No problems in open files", None, cx);
            return;
        }
        self.open_multibuffer("Problems".into(), targets, Marks::None, window, cx);
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
