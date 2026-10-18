//! Language servers: events from editors, navigation results, installs.

use super::*;

impl Workspace {
    // ---- Language servers -----------------------------------------------------------

    pub(super) fn on_editor_lsp_event(&mut self, event: &EditorLspEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EditorLspEvent::Navigate { title, locations, encoding, panel: true } => {
                self.show_usages(title.clone(), locations.clone(), *encoding, window, cx)
            }
            EditorLspEvent::Navigate { title, locations, encoding, panel: false } => match locations.as_slice() {
                [one] => self.open_location(one.clone(), *encoding, window, cx),
                many => self.show_locations(title.clone(), many.to_vec(), *encoding, window, cx),
            },
            EditorLspEvent::Rename { server, path, position, current } => {
                let pending = PendingInput::RenameSymbol {
                    server: server.clone(),
                    path: path.clone(),
                    position: *position,
                    current: current.clone(),
                };
                self.ask_for_name(pending, window, cx);
            }
            EditorLspEvent::CodeActions { actions, position } => {
                let entries = actions
                    .iter()
                    .map(|(_, a)| if a.preferred { MenuEntry::item(a.title.clone(), Icon::Check) } else { MenuEntry::text(a.title.clone()) })
                    .collect();
                let menu_actions = actions.iter().map(|(server, a)| Some(MenuAction::CodeAction(server.clone(), a.clone()))).collect();
                self.show_context_menu(entries, menu_actions, *position, None, window, cx);
            }
            EditorLspEvent::Message(message) => self.toast(ToastKind::Info, message.clone(), None, cx),
            EditorLspEvent::Command { server, command } => self.execute_command(server.clone(), command.clone(), cx),
            EditorLspEvent::Hierarchy { kind, server, items } => {
                let root = self.root.clone();
                let label: crate::hierarchy::Labeler = std::rc::Rc::new(move |path| location_label(root.as_deref(), path));
                let name = items[0]["name"].as_str().unwrap_or_default().to_string();
                let nodes = crate::hierarchy::roots(*kind, server.clone(), items.clone(), label);
                self.show_results(format!("{} of {name}", kind.name()).into(), nodes, window, cx);
            }
        }
    }

    /// The open tab for `path`, comparing real paths (servers may report
    /// `/tmp/…` for `/private/tmp/…`): `path`'s is read once, the tabs' are
    /// kept (`real_paths`), each read again only when its path changes.
    pub(super) fn tab_for(&self, path: &Path, cx: &App) -> Option<Entity<Editor>> {
        let path_of = |tab: &Entity<Editor>| tab.read(cx).doc.read(cx).path.clone();
        if let Some(tab) = self.editors.iter().find(|t| path_of(t).as_deref() == Some(path)) {
            return Some(tab.clone());
        }
        let wanted = crate::paths::canonicalize(path).ok()?;
        let mut real_paths = self.real_paths.borrow_mut();
        if real_paths.len() > 2 * self.editors.len() {
            let open: HashSet<EntityId> = self.editors.iter().map(|t| t.entity_id()).collect();
            real_paths.retain(|id, _| open.contains(id));
        }
        self.editors
            .iter()
            .find(|tab| {
                let Some(path) = path_of(tab) else { return false };
                let (known, real) = real_paths.entry(tab.entity_id()).or_insert_with(|| (PathBuf::new(), None));
                if *known != path {
                    *real = crate::paths::canonicalize(&path).ok();
                    *known = path;
                }
                real.as_deref() == Some(wanted.as_path())
            })
            .cloned()
    }

    pub(super) fn open_location(&mut self, location: Location, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(location.path.clone(), window, cx);
        if let Some(editor) = self.tab_for(&location.path, cx) {
            editor.update(cx, |e, cx| {
                let offset = protocol::position_to_offset(e.doc.read(cx).rope(), location.range.start, encoding);
                e.go_to(offset, window, cx);
            });
        }
    }

    /// Each location's line (as it reads today, untrimmed): from the tab's
    /// text where the file is open, else read in the background, only as far
    /// as the last line wanted.
    fn location_lines(&self, locations: &[Location], cx: &App) -> Task<Vec<String>> {
        let mut tabs: HashMap<PathBuf, Option<Entity<Editor>>> = HashMap::new();
        // Per file not open: the lines wanted, and the locations wanting them.
        let mut unread: HashMap<PathBuf, Vec<(usize, usize)>> = HashMap::new();
        let mut lines = Vec::with_capacity(locations.len());
        for (ix, location) in locations.iter().enumerate() {
            let line = location.range.start.line as usize;
            let tab = tabs.entry(location.path.clone()).or_insert_with(|| self.tab_for(&location.path, cx));
            lines.push(match tab {
                Some(editor) => editor.read(cx).doc.read(cx).rope().get_line(line).map(|l| l.to_string()).unwrap_or_default(),
                None => {
                    unread.entry(location.path.clone()).or_default().push((line, ix));
                    String::new()
                }
            });
        }
        cx.background_executor().spawn(async move {
            for (path, wanted) in unread {
                let read = read_lines(&path, wanted.iter().map(|(line, _)| *line).max().unwrap_or(0));
                for (line, ix) in wanted {
                    lines[ix] = read.get(line).cloned().unwrap_or_default();
                }
            }
            lines
        })
    }

    /// Lists usages in the Find panel, grouped by file.
    pub(super) fn show_usages(&mut self, title: SharedString, locations: Vec<Location>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let read = self.location_lines(&locations, cx);
        self.locations_task = Some(cx.spawn_in(window, async move |ws, cx| {
            let lines = read.await;
            ws.update_in(cx, |ws, window, cx| {
                let nodes = crate::results_panel::usages_tree(locations, lines, encoding, |path| ws.location_label(path));
                ws.show_results(title, nodes, window, cx);
            })
            .ok();
        }));
    }

    /// Lists usages (or several declarations) to pick from, each with its
    /// line.
    pub(super) fn show_locations(&mut self, title: SharedString, locations: Vec<Location>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let read = self.location_lines(&locations, cx);
        let mut rows: Vec<LocationRow> = locations
            .into_iter()
            .map(|location| {
                let line = location.range.start.line as usize + 1;
                LocationRow { file: self.location_label(&location.path), line, preview: String::new(), location, encoding }
            })
            .collect();
        self.locations_task = Some(cx.spawn_in(window, async move |ws, cx| {
            let lines = read.await;
            ws.update_in(cx, |ws, window, cx| {
                for (row, line) in rows.iter_mut().zip(lines) {
                    row.preview = line.trim().to_string();
                }
                ws.close_modal(window, cx);
                ws.open_picker(PickerMode::Locations, window, cx);
                if let Some(Modal::Picker(picker)) = &ws.modals.current {
                    picker.update(cx, |p, cx| p.set_locations(Some(title), rows, cx));
                }
            })
            .ok();
        }));
    }

    /// Applies a language server's edits: in a file's tab if it has one,
    /// else in the document store without opening a tab (listed to save,
    /// see `edited_in_background`); each file's edits are one undo step.
    /// Edits computed for an older version of a file (a slow rename while
    /// typing) are read in that version's text and moved to today's; a file
    /// they can't be applied to is left alone. Resolves to whether all applied.
    pub(super) fn apply_workspace_edit(&mut self, edits: Vec<protocol::FileEdits>, encoding: Encoding, cx: &mut Context<Self>) -> Task<bool> {
        let loads: Vec<_> = edits.into_iter().map(|file| (self.document_to_edit(&file.path, cx), file)).collect();
        cx.spawn(async move |this, cx| {
            let mut docs = Vec::with_capacity(loads.len());
            for (load, file) in loads {
                docs.push((load.await, file));
            }
            this.update(cx, |ws, cx| {
                let (mut skipped, mut background) = (Vec::new(), Vec::new());
                for (doc, file) in docs {
                    let Some(resolved) = doc.as_ref().ok().and_then(|doc| resolve_file_edits(doc.read(cx), &file, encoding)) else {
                        skipped.push(file.path);
                        continue;
                    };
                    let doc = doc.expect("resolved above");
                    if ws.edit_document(&doc, resolved, cx) {
                        background.push(doc);
                    }
                }
                ws.edited_in_background(background, cx);
                if !skipped.is_empty() {
                    let names: Vec<String> = skipped.iter().map(|p| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())).collect();
                    ws.toast(ToastKind::Error, "Some changes weren't applied", Some(format!("{} changed meanwhile or couldn't be opened", names.join(", "))), cx);
                }
                skipped.is_empty()
            })
            .unwrap_or(false)
        })
    }

    pub(super) fn rename_symbol(&mut self, server: Arc<Server>, path: PathBuf, position: Position, new_name: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = serde_json::json!({
            "textDocument": { "uri": protocol::path_to_uri(&path) },
            "position": position.to_json(),
            "newName": new_name,
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = server.request("textDocument/rename", params).await;
            this.update(cx, |ws, cx| match result {
                Ok(value) => {
                    let edits = protocol::workspace_edit(&value);
                    let (files, count) = (edits.len(), edits.iter().map(|f| f.edits.len()).sum::<usize>());
                    if count == 0 {
                        ws.toast(ToastKind::Info, "Nothing to rename here", None, cx);
                    } else {
                        let applied = ws.apply_workspace_edit(edits, server.encoding, cx);
                        cx.spawn(async move |ws, cx| {
                            if applied.await {
                                let files = if files == 1 { "1 file".to_string() } else { format!("{files} files") };
                                let message = format!("Renamed {count} occurrence{} in {files}", plural(count));
                                ws.update(cx, |ws, cx| ws.toast(ToastKind::Success, message, None, cx)).ok();
                            }
                        })
                        .detach();
                    }
                }
                // The server dropped it (the text changed meanwhile): not an error.
                Err(err) if err.is_cancelled() => {}
                Err(err) => ws.toast(ToastKind::Error, "Couldn't rename", Some(err.message), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Runs a chosen code action: its edit, then its command; actions that
    /// carry neither are resolved first.
    pub(super) fn apply_code_action(&mut self, server: Arc<Server>, action: serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
        // A bare Command rather than a CodeAction.
        if action.get("command").and_then(serde_json::Value::as_str).is_some() {
            self.execute_command(server, action, cx);
            return;
        }
        let (edit, command) = (action.get("edit"), action.get("command"));
        if edit.is_none() && command.is_none() {
            if !server.resolves("codeActionProvider") {
                return;
            }
            cx.spawn_in(window, async move |this, cx| {
                let resolved = server.request("codeAction/resolve", action).await;
                this.update_in(cx, |ws, window, cx| match resolved {
                    Ok(resolved) if resolved.get("edit").is_some() || resolved.get("command").is_some() => {
                        ws.apply_code_action(server, resolved, window, cx);
                    }
                    Ok(_) => {}
                    Err(err) if err.is_cancelled() => {}
                    Err(err) => ws.toast(ToastKind::Error, "Couldn't apply the action", Some(err.message), cx),
                })
                .ok();
            })
            .detach();
            return;
        }
        if let Some(edit) = edit {
            self.apply_workspace_edit(protocol::workspace_edit(edit), server.encoding, cx).detach();
        }
        if let Some(command) = command.cloned() {
            self.execute_command(server, command, cx);
        }
    }

    /// Asks the server to run a command; it may answer with edits
    /// (`workspace/applyEdit`), which the LSP store forwards.
    pub(super) fn execute_command(&mut self, server: Arc<Server>, command: serde_json::Value, cx: &mut Context<Self>) {
        let params = serde_json::json!({
            "command": command.get("command").cloned().unwrap_or_default(),
            "arguments": command.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([])),
        });
        cx.spawn(async move |this, cx| {
            if let Err(err) = server.request("workspace/executeCommand", params).await
                && !err.is_cancelled()
            {
                this.update(cx, |ws, cx| ws.toast(ToastKind::Error, "The action failed", Some(err.message), cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn restart_language_server(&mut self, _: &RestartLanguageServer, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_editor().and_then(|e| e.read(cx).doc.read(cx).path.clone()) else { return };
        self.lsp.update(cx, |s, cx| s.restart(&path, cx));
    }

    /// Settings… opens the Preferences window (or brings it forward).
    pub(super) fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        cx.defer(|cx| preferences::open(None, cx));
    }

    /// Extensions…: the Extensions page of Preferences.
    pub(super) fn open_extensions(&mut self, _: &OpenExtensions, _: &mut Window, cx: &mut Context<Self>) {
        cx.defer(|cx| preferences::open(Some(preferences::Page::Extensions), cx));
    }

    pub(super) fn reload_extensions(&mut self, _: &ReloadExtensions, _: &mut Window, cx: &mut Context<Self>) {
        extensions::reload(cx);
        self.toast(ToastKind::Success, "Extensions reloaded", None, cx);
    }

    pub(super) fn open_settings_file(&mut self, _: &OpenSettingsFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(Settings::path(), settings::SETTINGS_TEMPLATE, window, cx);
    }

    /// The project's `.kuana/settings.json`, created from a template when
    /// it has none.
    pub(super) fn open_project_settings(&mut self, _: &OpenProjectSettings, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            self.toast(ToastKind::Info, "Open a folder to give it settings", None, cx);
            return;
        };
        self.open_config_file(Some(root.join(crate::project_settings::FILE)), crate::project_settings::TEMPLATE, window, cx);
    }

    pub(super) fn open_keymap(&mut self, _: &OpenKeymap, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(keymap::keymap_path(), keymap::KEYMAP_TEMPLATE, window, cx);
    }
}

/// A file's lines up to `last` (0-based): read only that far.
fn read_lines(path: &Path, last: usize) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else { return Vec::new() };
    let mut reader = std::io::BufReader::new(file);
    let (mut lines, mut line) = (Vec::new(), Vec::new());
    while lines.len() <= last {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => lines.push(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_string()),
        }
    }
    lines
}

/// A file's edits as byte ranges in the document's text today, or None when
/// they were computed for a version that's no longer known.
fn resolve_file_edits(doc: &Document, file: &protocol::FileEdits, encoding: Encoding) -> Option<Vec<(std::ops::Range<usize>, String)>> {
    match file.version {
        Some(version) if version != doc.version() => {
            let text = doc.text_at_lsp_version(version)?;
            protocol::resolve_edits(text, &file.edits, encoding)
                .into_iter()
                .map(|(range, text)| Some((doc.offset_since(version, range.start)?..doc.offset_since(version, range.end)?, text)))
                .collect()
        }
        _ => Some(protocol::resolve_edits(doc.rope(), &file.edits, encoding)),
    }
}

#[cfg(test)]
mod tests {
    /// A usage's line is read from the file, only that far; a file that
    /// isn't UTF-8 still shows, a missing one shows nothing.
    #[test]
    fn reads_lines_only_as_far_as_wanted() {
        let dir = std::env::temp_dir().join(format!("kuana-usages-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, b"fn a() {}\r\n    let x = caf\xe9;\nlast").unwrap();
        assert_eq!(super::read_lines(&path, 1), ["fn a() {}", "    let x = caf\u{fffd};"]);
        assert_eq!(super::read_lines(&path, 9), ["fn a() {}", "    let x = caf\u{fffd};", "last"]);
        assert_eq!(super::read_lines(&path, 0).len(), 1);
        assert!(super::read_lines(&dir.join("gone.rs"), 3).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
