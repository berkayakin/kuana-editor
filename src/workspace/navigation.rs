//! Search and navigation: pickers, find, Go to.

use super::*;

impl Workspace {
    // ---- Search & navigation ---------------------------------------------------

    pub(super) fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_bar(false, window, cx);
    }

    pub(super) fn replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_bar(true, window, cx);
    }

    pub(super) fn show_find_bar(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        self.close_modal(window, cx);
        self.show_find = true;
        self.find_bar.update(cx, |f, cx| f.show(editor, replace, window, cx));
        cx.notify();
    }

    pub(super) fn find_next(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, window, cx);
    }

    pub(super) fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(false, window, cx);
    }

    pub(super) fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        if !self.find_bar.read(cx).has_query(cx) {
            self.show_find_bar(false, window, cx);
            return;
        }
        self.find_bar.update(cx, |f, cx| {
            f.set_target(Some(editor), cx);
            if forward { f.select_next(cx) } else { f.select_prev(cx) }
        });
    }

    pub(super) fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.show_find {
            self.find_bar.update(cx, |f, cx| f.dismiss(cx));
        } else {
            cx.propagate();
        }
    }

    pub(super) fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving the theme picker without confirming undoes the preview.
        if let Some(Modal::Theme(picker)) = &self.modals.current {
            let picker = picker.read(cx);
            if !picker.confirmed {
                let original = picker.original.clone();
                theme::apply_theme(&original, cx);
            }
        }
        if self.modals.take().is_some() {
            self.focus_active(window, cx);
            self.ask_waiting_trust(window, cx);
            cx.notify();
        }
    }

    pub(super) fn open_picker(&mut self, mode: PickerMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Picker(p)) = &self.modals.current
            && p.read(cx).mode() == mode
        {
            // Pressing the shortcut again closes it, like JetBrains.
            self.close_modal(window, cx);
            return;
        }
        let files = if self.root.is_some() {
            self.project.openable()
        } else {
            Arc::new(self.items(cx).iter().filter(|t| t.is_file()).filter_map(|t| t.path(cx)).collect())
        };
        let recent = if mode == PickerMode::Projects { self.recent_projects.clone() } else { self.recent.clone() };
        let (root, actions) = (self.root.clone(), self.actions.clone());
        let picker = cx.new(|cx| Picker::new(mode, root, files, recent, actions, window, cx));
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::OpenFile(path) => {
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
            }
            PickerEvent::OpenProject { path, new_window } => {
                ws.close_modal(window, cx);
                ws.open_recent_project(path.clone(), *new_window, window, cx);
            }
            PickerEvent::RunAction(action) => {
                ws.close_modal(window, cx);
                window.dispatch_action(action.boxed_clone(), cx);
            }
            PickerEvent::Run(target) => {
                ws.close_modal(window, cx);
                ws.run_target(target.clone(), window, cx);
            }
            PickerEvent::Debug(target) => {
                ws.close_modal(window, cx);
                ws.debug_target(target.clone(), window, cx);
            }
            PickerEvent::OpenLocation { location, encoding } => {
                ws.close_modal(window, cx);
                ws.open_location(location.clone(), *encoding, window, cx);
            }
            PickerEvent::GoTo(offset) => {
                ws.close_modal(window, cx);
                if let Some(editor) = ws.active_editor(cx) {
                    editor.update(cx, |e, cx| e.go_to(*offset, window, cx));
                }
            }
            PickerEvent::SymbolQuery(query) => ws.query_symbols(query.clone(), cx),
            PickerEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    pub(super) fn select_theme(&mut self, _: &SelectTheme, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modals.current, Some(Modal::Theme(_))) {
            self.close_modal(window, cx);
            return;
        }
        self.close_modal(window, cx);
        let picker = cx.new(|cx| ThemePicker::new(window, cx));
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &ThemePickerEvent, window, cx| match event {
            ThemePickerEvent::Confirmed(name) => {
                if let Err(err) = Settings::set("theme", name.as_str().into()) {
                    let _ = window.prompt(PromptLevel::Warning, "Couldn't save the theme choice", Some(&err.to_string()), &["OK"], cx);
                }
                ws.check_config(cx);
                ws.close_modal(window, cx);
            }
            ThemePickerEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Theme(picker), subscription);
        cx.notify();
    }

    pub(super) fn open_themes_folder(&mut self, _: &OpenThemesFolder, window: &mut Window, cx: &mut Context<Self>) {
        match theme::ensure_themes_dir() {
            Ok(dir) => cx.open_url(&format!("file://{}", dir.display())),
            Err(err) => {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the themes folder", Some(&err.to_string()), &["OK"], cx);
            }
        }
    }

    pub(super) fn open_snippets_folder(&mut self, _: &OpenSnippetsFolder, window: &mut Window, cx: &mut Context<Self>) {
        match crate::snippets::ensure_snippets_dir() {
            Ok(dir) => cx.open_url(&format!("file://{}", dir.display())),
            Err(err) => {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the snippets folder", Some(&err.to_string()), &["OK"], cx);
            }
        }
    }

    pub(super) fn reload_themes(&mut self, _: &ReloadThemes, _: &mut Window, cx: &mut Context<Self>) {
        theme::reload_themes(cx);
    }

    pub(super) fn search_everywhere(&mut self, _: &SearchEverywhere, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Everywhere, window, cx);
    }

    pub(super) fn go_to_file(&mut self, _: &GoToFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Files, window, cx);
    }

    pub(super) fn recent_files(&mut self, _: &RecentFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Recent, window, cx);
    }

    pub(super) fn find_action(&mut self, _: &FindAction, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Actions, window, cx);
    }

    /// Go to Symbol: symbols across the project, from the running language servers.
    pub(super) fn go_to_symbol(&mut self, _: &GoToSymbol, window: &mut Window, cx: &mut Context<Self>) {
        let servers = self.lsp.read(cx).running_servers();
        if !servers.iter().any(|s| s.capability("workspaceSymbolProvider")) {
            self.toast(ToastKind::Info, "No language server to ask", Some("Go to Symbol uses the project's language servers. Open one of its source files first.".into()), cx);
            return;
        }
        self.open_picker(PickerMode::Symbols, window, cx);
    }

    pub(super) fn query_symbols(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_string();
        let servers: Vec<_> = self.lsp.read(cx).running_servers().into_iter().filter(|s| s.capability("workspaceSymbolProvider")).collect();
        if query.is_empty() || servers.is_empty() {
            self.symbol_task = None;
            self.set_symbol_rows(Vec::new(), cx);
            return;
        }
        self.symbol_task = Some(cx.spawn(async move |this, cx| {
            // Typing fast sends one request, not one per key.
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let requests = servers.iter().map(|server| server.request("workspace/symbol", serde_json::json!({ "query": query })));
            let results = futures::future::join_all(requests).await;
            this.update(cx, |ws, cx| {
                let mut rows = Vec::new();
                for (server, result) in servers.iter().zip(results) {
                    for (name, container, location) in result.map(|v| protocol::workspace_symbols(&v)).unwrap_or_default() {
                        let preview = match container {
                            Some(container) => format!("{name}  ·  {container}"),
                            None => name,
                        };
                        let file = ws.location_label(&location.path);
                        let line = location.range.start.line as usize + 1;
                        rows.push(LocationRow { location, file, line, preview, encoding: server.encoding });
                    }
                }
                ws.set_symbol_rows(rows, cx);
            })
            .ok();
        }));
    }

    pub(super) fn set_symbol_rows(&mut self, rows: Vec<LocationRow>, cx: &mut Context<Self>) {
        if let Some(Modal::Picker(picker)) = &self.modals.current
            && picker.read(cx).mode() == PickerMode::Symbols
        {
            picker.update(cx, |p, cx| p.set_locations(None, rows, cx));
        }
    }

    pub(super) fn file_structure(&mut self, _: &FileStructure, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let (symbols, cursor) = {
            let e = editor.read(cx);
            let symbols: Vec<_> = e
                .doc
                .read(cx)
                .syntax()
                .symbols(e.doc.read(cx).rope())
                .into_iter()
                .map(|s| {
                    let line = e.doc.read(cx).row_of(s.offset) + 1;
                    (s, line)
                })
                .collect();
            (symbols, e.primary_selection().head)
        };
        self.open_picker(PickerMode::Structure, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modals.current
            && picker.read(cx).mode() == PickerMode::Structure
        {
            picker.update(cx, |p, cx| p.set_symbols(symbols, cursor, cx));
        }
    }

    pub(super) fn go_to_line(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let (line_count, current) = {
            let e = editor.read(cx);
            (e.doc.read(cx).line_count(), e.cursor_position(cx))
        };
        let modal = cx.new(|cx| GoToLineModal::new(line_count, current, window, cx));
        let subscription = cx.subscribe_in(&modal, window, move |ws, _, event: &GoToLineEvent, window, cx| match event {
            GoToLineEvent::Confirm { line, column } => {
                ws.close_modal(window, cx);
                let (line, column) = (*line, *column);
                editor.update(cx, |e, cx| {
                    let row = line - 1;
                    let start = e.doc.read(cx).line_start(row);
                    let text = e.doc.read(cx).line_text(row);
                    let col = text.char_indices().nth(column - 1).map_or(text.len(), |(b, _)| b);
                    e.go_to(start + col, window, cx);
                });
            }
            GoToLineEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::GoToLine(modal), subscription);
        cx.notify();
    }

    /// Current text of every open document, so searches see unsaved edits
    /// (those made without a tab too).
    pub(super) fn open_buffers(&self, cx: &App) -> HashMap<PathBuf, Arc<str>> {
        // Changed without a tab (kept only while unsaved).
        let background = self.documents.read(cx).kept(cx).into_iter().filter_map(|doc| {
            let d = doc.read(cx);
            Some((d.path.clone()?, Arc::from(d.rope().to_string())))
        });
        self.editors
            .iter()
            .filter_map(|t| {
                let e = t.read(cx);
                let text = match e.unloaded() {
                    // Not read yet: its unsaved text, else the file on disk.
                    Some(u) => u.unsaved.as_ref()?.to_string(),
                    // Saved: the search reads the same text from disk, in the
                    // background, instead of a copy made here of every tab.
                    None if !e.doc.read(cx).is_dirty() => return None,
                    None => e.doc.read(cx).rope().to_string(),
                };
                Some((e.doc.read(cx).path.clone()?, Arc::from(text)))
            })
            .chain(background)
            .collect()
    }

    pub(super) fn find_in_files(&mut self, _: &FindInFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search(false, window, cx);
    }

    pub(super) fn replace_in_files(&mut self, _: &ReplaceInFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search(true, window, cx);
    }

    pub(super) fn open_project_search(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search_in(None, replace, window, cx);
    }

    /// Find in Files, optionally limited to one folder.
    pub(super) fn open_project_search_in(&mut self, scope: Option<PathBuf>, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let root = scope
            .or_else(|| self.root.clone())
            .or_else(|| {
                let path = self.active_editor(cx)?.read(cx).doc.read(cx).path.clone()?;
                path.parent().map(Path::to_path_buf)
            })
            .or_else(|| std::env::current_dir().ok());
        let Some(root) = root else { return };
        let initial = self.active_editor(cx).and_then(|e| {
            let e = e.read(cx);
            let s = e.primary_selection();
            let text = e.doc.read(cx).slice(s.range());
            (!s.is_empty() && !text.contains('\n')).then_some(text)
        });
        let overrides = self.open_buffers(cx);
        // The scanned project's files, if the search is in it.
        let scanned = !self.project.root().as_os_str().is_empty() && root.starts_with(self.project.root());
        let project = scanned.then(|| self.project.clone());
        let search = cx.new(|cx| ProjectSearch::new(root, project, initial, replace, overrides, window, cx));
        let subscription = cx.subscribe_in(&search, window, |ws, _, event: &ProjectSearchEvent, window, cx| match event {
            ProjectSearchEvent::Open { path, range } => {
                let range = range.clone();
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
                if let Some(editor) = ws.active_editor(cx) {
                    editor.update(cx, |e, cx| e.select_range(range, cx));
                }
            }
            ProjectSearchEvent::ReplaceAll { query, replacement, files } => {
                ws.confirm_replace_in_files(query.clone(), replacement.clone(), files.clone(), window, cx);
            }
            ProjectSearchEvent::Dismissed => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::ProjectSearch(search), subscription);
        cx.notify();
    }

    pub(super) fn confirm_replace_in_files(
        &mut self,
        query: SearchQuery,
        replacement: String,
        files: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if files.is_empty() {
            return;
        }
        let message = format!(
            "Replace all occurrences in {} file{}?",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        // Files that aren't open change without a tab: undoable once opened,
        // until they're saved (auto-save saves them right away).
        let detail = match self.auto_save {
            AutoSave::Off => "Files that aren't open stay unsaved until you save them (from the status bar). Changes can be undone.",
            _ => "Changes to open files can be undone. Other files are saved right away.",
        };
        let rx = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Replace All", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if rx.await != Ok(0) {
                return;
            }
            let Ok(loads) = this.update(cx, |ws, cx| files.iter().map(|path| (path.clone(), ws.document_to_edit(path, cx))).collect::<Vec<_>>()) else {
                return;
            };
            let mut docs = Vec::with_capacity(loads.len());
            for (path, load) in loads {
                docs.push((path, load.await));
            }
            this.update_in(cx, |ws, window, cx| {
                let (mut failures, mut background) = (Vec::new(), Vec::new());
                for (path, doc) in docs {
                    let doc = match doc {
                        Ok(doc) => doc,
                        Err(err) => {
                            failures.push(format!("{}: {err}", path.display()));
                            continue;
                        }
                    };
                    let text = doc.read(cx).rope().to_string();
                    let edits = query.find_all(&text).into_iter().map(|r| (r.clone(), query.replacement_for(&text, r, &replacement))).collect();
                    if ws.edit_document(&doc, edits, cx) {
                        background.push(doc);
                    }
                }
                ws.edited_in_background(background, cx);
                if !failures.is_empty() {
                    let _ = window.prompt(PromptLevel::Critical, "Some files couldn't be changed", Some(&failures.join("\n")), &["OK"], cx);
                }
                let overrides = ws.open_buffers(cx);
                if let Some(Modal::ProjectSearch(search)) = &ws.modals.current {
                    search.update(cx, |s, cx| s.refresh(overrides, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Detects a double tap of Shift (Search Everywhere), and tells the shown
    /// editors about ⌘/Ctrl (links under the pointer): only the focused one
    /// hears it, and the pointer may rest on another.
    pub(super) fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Ctrl+Tab's keys let go: the tab it got to is the one in use.
        if !event.modifiers.modified() {
            self.tab_switch = None;
        }
        let shown: Vec<Entity<Editor>> = self.center.panes().iter().filter_map(|p| p.read(cx).active_item()?.editor().cloned()).collect();
        for editor in shown {
            editor.update(cx, |e, cx| e.secondary_modifier_changed(event.modifiers.secondary(), window, cx));
        }
        let m = event.modifiers;
        if m.shift && !m.control && !m.alt && !m.platform && !m.function {
            self.shift_down = true;
            return;
        }
        if !m.modified() && std::mem::take(&mut self.shift_down) {
            let now = Instant::now();
            if self.last_shift_tap.is_some_and(|t| now.duration_since(t) < DOUBLE_SHIFT_WINDOW) {
                self.last_shift_tap = None;
                self.open_picker(PickerMode::Everywhere, window, cx);
            } else {
                self.last_shift_tap = Some(now);
            }
            return;
        }
        self.shift_down = false;
        self.last_shift_tap = None;
    }

    /// Any real key press cancels a pending double-Shift.
    pub(super) fn on_any_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, _: &mut Context<Self>) {
        self.shift_down = false;
        self.last_shift_tap = None;
    }
}
