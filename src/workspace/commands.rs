//! Workspace actions (opening, saving, settings and keymap files).

use super::*;

impl Workspace {
    // ---- Actions ------------------------------------------------------------------

    pub(super) fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| Editor::new(Document::empty(), cx));
        self.add_tab(editor.into(), window, cx);
    }

    pub(super) fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    for p in paths {
                        ws.open_path(p, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_folder(OpenIn::Auto, window, cx);
    }

    pub(super) fn open_folder_in_this_window(&mut self, _: &OpenFolderInThisWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_folder(OpenIn::ThisWindow, window, cx);
    }

    pub(super) fn prompt_for_folder(&mut self, target: OpenIn, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |ws, window, cx| ws.open_project(path, target, window, cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<Self>) {
        open_window(Some(window.bounds()), cx, |_, _, _| {});
    }

    pub(super) fn minimize_window(&mut self, _: &MinimizeWindow, window: &mut Window, _: &mut Context<Self>) {
        window.minimize_window();
    }

    pub(super) fn zoom_window(&mut self, _: &ZoomWindow, window: &mut Window, _: &mut Context<Self>) {
        window.zoom_window();
    }

    pub(super) fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.prepare_to_close(CloseIntent::CloseWindow, window, cx) {
            window.remove_window();
        }
    }

    /// Opens a project folder. A folder that's already open in another
    /// window brings that window to the front instead.
    pub(super) fn open_project(&mut self, path: PathBuf, target: OpenIn, window: &mut Window, cx: &mut Context<Self>) {
        let path = crate::paths::canonicalize(&path).unwrap_or(path);
        if !path.is_dir() {
            self.toast(ToastKind::Error, "Folder not found", Some(clone_dialog::display_path(&path)), cx);
            self.update_recent_projects(|list| list.retain(|p| p != &path), cx);
            cx.notify();
            return;
        }
        if self.root.as_deref() == Some(path.as_path()) {
            return;
        }
        if let Some(other) = window_with_project(&path, cx) {
            other.update(cx, |_, window, _| window.activate_window()).ok();
            return;
        }
        match target {
            OpenIn::Auto if self.root.is_none() => self.set_root(path, window, cx),
            OpenIn::ThisWindow => self.replace_project(path, window, cx),
            OpenIn::NewWindow | OpenIn::Auto => {
                open_window(Some(window.bounds()), cx, move |ws, window, cx| ws.set_root(path, window, cx));
            }
        }
    }

    /// Swaps this window's project for another one, closing its tabs once
    /// unsaved edits are saved or discarded. Without a project yet, lone
    /// files stay open.
    pub(super) fn replace_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.is_none() {
            self.set_root(path, window, cx);
        } else if self.prepare_to_close(CloseIntent::ReplaceProject(path.clone()), window, cx) {
            self.finish_close(CloseIntent::ReplaceProject(path), window, cx);
        }
    }

    /// Clears everything tied to the open project.
    pub(super) fn close_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.context_menu = None;
        self.pending_input = None;
        self.show_find = false;
        self.find_bar.update(cx, |f, cx| f.set_target(None, cx));
        self.editors.clear();
        self.tab_subscriptions.clear();
        self.pane_subscriptions.clear();
        let pane = self.new_pane(window, cx);
        self.center = PaneTree::new(pane.clone());
        self.active_pane = pane;
        self.session_task = None;
        self.terminals.update(cx, |p, cx| p.clear(cx));
        self.running.clear();
        self.hide_bottom(BottomPanel::Terminal);
        if let Some((session, _)) = self.debug.take() {
            session.update(cx, |s, cx| s.stop(cx));
        }
        self.debug_active = false;
        self.hide_bottom(BottomPanel::Debug);
        self.debug_location = None;
        self.breakpoints.clear();
        self.bookmarks.clear();
        self.reset_todos();
        // Searches, test results and Local History list the old project's files.
        self.results.update(cx, |p, cx| p.clear(cx));
        self.has_results = false;
        self.hide_bottom(BottomPanel::Results);
        self.last_tests = None;
        self.test_statuses = Arc::default();
        self.lsp.update(cx, |s, _| {
            s.shutdown_all();
            s.set_root(None);
        });
        self.auto_save_tasks.clear();
        self.auto_save_warned.clear();
        self.recent.clear();
        self.project = Arc::default();
        self.project_task = None;
        self.project_changes.clear();
        self.root_gone = false;
        self.watcher = None;
        self.watch_task = None;
        self.root = None;
        // Extensions' views for the project go, and their code hears it closed.
        self.refresh_extension_views(cx);
        self.show_sidebar = false;
        self.git.update(cx, |g, cx| g.close(cx));
        window.focus(&self.focus_handle);
        cx.notify();
    }

    pub(super) fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_item(cx).and_then(|i| i.excerpts().cloned()) {
            self.save_multibuffer(&view, window, cx);
            return;
        }
        let Some(editor) = self.active_editor(cx) else { return };
        self.save_one(editor, window, cx);
    }

    /// Saves one editor's file (Save As for an untitled one), asking first
    /// when it changed on disk.
    pub(super) fn save_one(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).path.is_none() {
            self.save_as(&SaveAs, window, cx);
            return;
        }
        if let Some(check) = self.confirm_overwrite(&editor, window, cx) {
            cx.spawn_in(window, async move |this, cx| {
                if check.await == OnDisk::Write {
                    this.update_in(cx, |ws, window, cx| ws.format_and_save(editor, window, cx)).ok();
                }
            })
            .detach();
            return;
        }
        self.format_and_save(editor, window, cx);
    }

    /// Save, once it's clear the file may be written.
    fn format_and_save(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        // Format on save: only for an explicit Save, like VS Code, so
        // auto-save never reformats mid-typing.
        if editor.read(cx).format_on_save(cx) {
            let task = editor.update(cx, |e, cx| e.format(false, cx));
            let timeout = cx.background_executor().timer(FORMAT_ON_SAVE_TIMEOUT);
            cx.spawn_in(window, async move |this, cx| {
                // A stuck formatter or server mustn't stop the save; dropping
                // the task cancels its request.
                let result = match futures::future::select(task, timeout).await {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right(_) => Err("The formatter took too long".to_string()),
                };
                this.update_in(cx, |ws, window, cx| {
                    if let Err(err) = result {
                        ws.toast(ToastKind::Info, "Saved without formatting", Some(err), cx);
                    }
                    ws.save_editor(&editor, window, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        self.save_editor(&editor, window, cx);
    }

    /// Saves a tab, in the background: its text is taken now and written
    /// off the UI thread (see `Editor::save_in_background`).
    pub(super) fn save_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let (id, path) = (editor.entity_id(), editor.read(cx).doc.read(cx).path.clone());
        let saving = editor.update(cx, |e, cx| e.save_in_background(cx));
        cx.spawn_in(window, async move |ws, cx| {
            let saved = saving.await;
            ws.update_in(cx, |ws, window, cx| {
                match saved {
                    Ok(()) => {
                        ws.auto_save_warned.remove(&id);
                        // A database object's file compiles into its database.
                        if let Some(editor) = ws.editors.iter().find(|e| e.entity_id() == id).cloned() {
                            ws.database_saved(&editor, window, cx);
                            ws.extension_file_saved(&editor, cx);
                        }
                        ws.check_config(cx);
                        // The session may hold the text as unsaved.
                        ws.schedule_session_save(cx);
                    }
                    Err(err) => {
                        let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
                    }
                }
                ws.files_written(path, cx);
            })
            .ok();
        })
        .detach();
    }
}

impl Workspace {
    // ---- Settings and keymap files -----------------------------------------------------

    /// Applies settings.json / keymap.json if they changed, reporting problems.
    pub(super) fn check_config(&mut self, cx: &mut Context<Self>) {
        let problems = settings::reload_if_changed(cx);
        self.report_config_problems(problems, cx);
    }

    pub(super) fn report_config_problems(&mut self, problems: Vec<String>, cx: &mut Context<Self>) {
        const SHOWN: usize = 3;
        for problem in problems.iter().take(SHOWN) {
            let (file, detail) = problem.split_once(": ").unwrap_or(("settings", problem));
            self.toast(ToastKind::Error, format!("Problem in {file}"), Some(detail.to_string()), cx);
        }
        if problems.len() > SHOWN {
            self.toast(ToastKind::Error, format!("{} more problems in your settings", problems.len() - SHOWN), None, cx);
        }
    }

    /// Opens a config file in a tab, creating it from `template` first.
    pub(super) fn open_config_file(&mut self, path: Option<PathBuf>, template: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = path else { return };
        if !path.exists() {
            let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, template));
            if let Err(err) = written {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the file", Some(&err.to_string()), &["OK"], cx);
                return;
            }
        }
        self.open_path(path, window, cx);
    }
}
