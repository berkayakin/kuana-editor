//! Saving, auto-save and closing tabs.

use super::*;

impl Workspace {
    // ---- Auto-save -------------------------------------------------------------------

    /// Restarts the idle timer for an edited tab in `after_delay` mode.
    pub(super) fn schedule_auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if self.auto_save != AutoSave::AfterDelay || !editor.read(cx).doc.read(cx).is_dirty() {
            return;
        }
        let delay = self.auto_save_delay;
        let target = editor.downgrade();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |ws, cx| {
                if let Some(editor) = target.upgrade() {
                    ws.auto_save(&editor, cx);
                }
            })
            .ok();
        });
        self.auto_save_tasks.insert(editor.entity_id(), task);
    }

    /// Writes a tab's edits to disk, in the background, if it has a file
    /// and unsaved changes (see `may_auto_save`).
    pub(super) fn auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if !self.may_auto_save(editor, cx) {
            return;
        }
        let (id, path) = (editor.entity_id(), editor.read(cx).doc.read(cx).path.clone());
        let saving = editor.update(cx, |e, cx| e.save_in_background(cx));
        cx.spawn(async move |ws, cx| {
            let saved = saving.await;
            ws.update(cx, |ws, cx| ws.auto_saved(id, path, saved, cx)).ok();
        })
        .detach();
    }

    /// The same here and now (closing: what's still unsaved afterwards
    /// decides what to ask).
    pub(super) fn auto_save_now(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if self.may_auto_save(editor, cx) {
            let path = editor.read(cx).doc.read(cx).path.clone();
            let saved = editor.update(cx, |e, cx| e.save(cx));
            self.auto_saved(editor.entity_id(), path, saved, cx);
        }
    }

    /// Whether auto-save writes a tab: unsaved, with a file. Files changed
    /// or deleted by another program are left alone (and reported once) so
    /// auto-save never clobbers them.
    fn may_auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> bool {
        let id = editor.entity_id();
        self.auto_save_tasks.remove(&id);
        let doc = editor.read(cx).doc.read(cx);
        if !doc.is_dirty() || doc.path.is_none() {
            return false;
        }
        if doc.changed_on_disk() {
            let name = doc.file_name();
            let deleted = doc.path.as_deref().is_some_and(|p| !p.exists());
            if self.auto_save_warned.insert(id) {
                let (title, detail) = if deleted {
                    (format!("{name} was deleted"), "Auto-save is paused for this file so it doesn't bring it back. Save it manually to keep your version.")
                } else {
                    (format!("{name} changed on disk"), "Auto-save is paused for this file so it doesn't overwrite the other change. Save it manually to keep your version.")
                };
                self.toast(ToastKind::Error, title, Some(detail.into()), cx);
            }
            return false;
        }
        true
    }

    /// Tab `id` was auto-saved to `path` (or not: reported once).
    fn auto_saved(&mut self, id: EntityId, path: Option<PathBuf>, saved: std::io::Result<()>, cx: &mut Context<Self>) {
        match saved {
            Ok(()) => {
                self.auto_save_warned.remove(&id);
                self.files_written(path, cx);
                self.check_config(cx);
                // The session may hold the text as unsaved.
                self.schedule_session_save(cx);
            }
            Err(err) => {
                let name = path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if self.auto_save_warned.insert(id) {
                    self.toast(ToastKind::Error, format!("Couldn't auto-save {name}"), Some(err.to_string()), cx);
                }
            }
        }
    }

    /// Auto-saves every tab and the documents edited without one: in the
    /// background, or here and now (`now`: closing, quitting).
    pub(super) fn auto_save_all(&mut self, now: bool, cx: &mut Context<Self>) {
        for editor in self.editors.clone() {
            if now {
                self.auto_save_now(&editor, cx);
            } else {
                self.auto_save(&editor, cx);
            }
        }
        if !self.documents.read(cx).kept(cx).is_empty() {
            self.save_background(true, now, cx);
        }
    }

    /// A save of the text as it is that's still being written finishes
    /// here (written again, now), so closing doesn't ask about it.
    pub(super) fn finish_saving(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).is_being_saved() {
            editor.update(cx, |e, cx| e.save(cx)).ok();
        }
    }

    pub(super) fn toggle_auto_save(&mut self, _: &ToggleAutoSave, window: &mut Window, cx: &mut Context<Self>) {
        self.auto_save = if self.auto_save == AutoSave::Off { AutoSave::AfterDelay } else { AutoSave::Off };
        if let Err(err) = Settings::set("auto_save", self.auto_save.key().into()) {
            let _ = window.prompt(PromptLevel::Warning, "Couldn't save the auto-save choice", Some(&err.to_string()), &["OK"], cx);
        }
        self.check_config(cx);
        if self.auto_save == AutoSave::Off {
            self.auto_save_tasks.clear();
            self.toast(ToastKind::Success, "Auto-save off", None, cx);
        } else {
            self.auto_save_all(false, cx);
            self.toast(ToastKind::Success, "Auto-save on", None, cx);
        }
    }

    pub(super) fn check_for_updates(&mut self, _: &CheckForUpdates, _: &mut Window, cx: &mut Context<Self>) {
        let check = updater::check(cx);
        cx.spawn(async move |this, cx| {
            let result = check.await;
            this.update(cx, |ws, cx| match result {
                Ok(updater::Check::UpToDate) => {
                    ws.toast(ToastKind::Success, format!("Kuana {} is up to date", env!("CARGO_PKG_VERSION")), None, cx)
                }
                Ok(updater::Check::Ready(version)) => ws.toast(
                    ToastKind::Success,
                    format!("Kuana {version} is ready"),
                    Some("Restart to Update in the status bar installs it.".into()),
                    cx,
                ),
                Err(err) => ws.toast(ToastKind::Error, "Couldn't check for updates", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Downloads in progress: the first one's name and a bar (how many more
    /// after it); each one's size, progress and speed in the tooltip, which
    /// keeps up while it's shown.
    pub(super) fn render_downloads(&self, theme: &Theme) -> Option<AnyElement> {
        let downloads = crate::downloads::active();
        let first = downloads.first()?;
        let mut label = first.label.clone();
        if downloads.len() > 1 {
            label.push_str(&format!(" +{}", downloads.len() - 1));
        }
        let track = Hsla::from(theme.text_muted).alpha(0.25);
        // Without a percentage yet, a short stub shows it's started.
        let fill = first.fraction.unwrap_or(0.04).max(0.04);
        Some(
            div()
                .id("downloads")
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(24.))
                .px_2()
                .text_color(theme.text_muted)
                .child(icon_el(Icon::Download, px(13.), theme.accent.into()))
                .child(label)
                .child(
                    div()
                        .w(px(64.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(track)
                        .child(div().h_full().rounded_full().bg(theme.accent).w(relative(fill))),
                )
                .tooltip(|_, cx| crate::ui::live_tooltip_view(crate::downloads::summary, cx))
                .into_any_element(),
        )
    }

    pub(super) fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = updater::restart(cx) {
            self.toast(ToastKind::Error, "Couldn't install the update", Some(err), cx);
        }
    }

    /// Saves what it can before the window closes, the app quits or the
    /// project is replaced. A project's unsaved edits go into its session, so
    /// that always proceeds. Without a project, returns false (and asks the
    /// user) when edits would still be lost: untitled files, files that
    /// couldn't be saved, or any edits with auto-save off.
    pub(super) fn prepare_to_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.auto_save != AutoSave::Off {
            self.auto_save_all(true, cx);
        }
        for editor in self.editors.clone() {
            self.finish_saving(&editor, cx);
        }
        // A project keeps unsaved edits in its session and brings them back
        // next time ("hot exit"), so there's nothing to ask.
        if self.root.is_some() {
            self.session_task = None;
            match self.save_session(cx) {
                Ok(()) => return true,
                Err(err) => eprintln!("couldn't save the session: {err}"),
            }
        }
        if self.close_confirmed && !matches!(intent, CloseIntent::ReplaceProject(_)) {
            return true;
        }
        let background = self.documents.read(cx).kept(cx);
        let unsaved: Vec<String> = self
            .editors
            .iter()
            .map(|t| t.read(cx).doc.read(cx))
            .chain(background.iter().map(|d| d.read(cx)))
            .filter(|d| d.is_dirty())
            .map(|d| d.file_name())
            .collect();
        if unsaved.is_empty() {
            return true;
        }
        let message = match unsaved.as_slice() {
            [one] => format!("Do you want to save the changes you made to {one}?"),
            many => format!("Do you want to save the changes you made to {} files?", many.len()),
        };
        let rx = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Your changes will be lost if you don't save them."),
            &["Save All", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = rx.await else { return };
            this.update_in(cx, |ws, window, cx| match answer {
                0 => {
                    ws.save_background(false, true, cx);
                    for editor in ws.editors.clone() {
                        if editor.read(cx).doc.read(cx).is_dirty()
                            && editor.read(cx).doc.read(cx).path.is_some()
                            && let Err(err) = editor.update(cx, |e, cx| e.save(cx))
                        {
                            let name = editor.read(cx).doc.read(cx).file_name();
                            ws.toast(ToastKind::Error, format!("Couldn't save {name}"), Some(err.to_string()), cx);
                        }
                    }
                    // Untitled files need a name first; show the first one.
                    if let Some(dirty) = ws.editors.iter().find(|t| t.read(cx).doc.read(cx).is_dirty()).cloned() {
                        ws.activate_editor(&dirty, window, cx);
                        if dirty.read(cx).doc.read(cx).path.is_none() {
                            ws.save_as(&SaveAs, window, cx);
                        }
                    } else if ws.documents.read(cx).kept(cx).is_empty() {
                        ws.finish_close(intent, window, cx);
                    }
                }
                1 => ws.finish_close(intent, window, cx),
                // Windows that already agreed to quit must ask again next time.
                _ if matches!(intent, CloseIntent::Quit) => cx.defer(cancel_quit),
                _ => {}
            })
            .ok();
        })
        .detach();
        false
    }

    pub(super) fn finish_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) {
        match intent {
            CloseIntent::CloseWindow => {
                self.close_confirmed = true;
                window.remove_window();
            }
            CloseIntent::Quit => {
                self.close_confirmed = true;
                // Go on to the next window with unsaved edits.
                cx.defer(quit);
            }
            CloseIntent::ReplaceProject(path) => {
                self.close_project(window, cx);
                self.set_root(path, window, cx);
            }
        }
    }

    pub(super) fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.active_item(cx) else { return };
        let current = item.path(cx);
        let dir = current
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| self.root.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let saving = item.read(cx).saving();
        if saving == Saving::Nothing {
            return;
        }
        let name = item.tab_title(cx);
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    let saved = path.clone();
                    let result = match &item {
                        PaneItem::Editor(editor) => {
                            let result = editor.update(cx, |e, cx| {
                                e.set_path(path, cx);
                                e.save(cx)
                            });
                            // The store finds the document by its new path.
                            let doc = editor.read(cx).doc.clone();
                            ws.documents.update(cx, |s, cx| s.add(&doc, cx));
                            match &current {
                                Some(old) => ws.lsp.update(cx, |s, cx| s.moved(old, editor, cx)),
                                None => ws.lsp.update(cx, |s, cx| s.register(editor, cx)),
                            }
                            result
                        }
                        // The picture as it is on disk, under the new name.
                        PaneItem::Image(image) => match current.as_deref().map(|from| std::fs::copy(from, &path)) {
                            Some(Ok(_)) => {
                                image.update(cx, |i, cx| i.set_path(path, cx));
                                Ok(())
                            }
                            Some(Err(err)) => Err(err),
                            None => Ok(()),
                        },
                        PaneItem::Preview(_) => Ok(()),
                    };
                    if let Err(err) = result {
                        let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
                    }
                    ws.tree.update(cx, |t, cx| t.paths_changed([saved.as_path()], cx));
                    ws.schedule_session_save(cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item(cx) {
            self.close_item(&item, window, cx);
        }
    }

    /// Closes a tab, asking first about unsaved changes it would lose.
    pub(super) fn close_item(&mut self, item: &PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            PaneItem::Editor(editor) => self.close_editor(editor, window, cx),
            // Nothing of their own to lose.
            PaneItem::Image(_) | PaneItem::Preview(_) => self.remove_item(item, window, cx),
        }
    }

    pub(super) fn close_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let editor = editor.clone();
        if self.auto_save != AutoSave::Off {
            self.auto_save_now(&editor, cx);
        }
        self.finish_saving(&editor, cx);
        // Another tab still shows the document: nothing is lost.
        if editor.read(cx).doc.read(cx).is_dirty() && self.siblings(&editor, cx).is_empty() {
            let name = editor.read(cx).doc.read(cx).file_name();
            let rx = window.prompt(
                PromptLevel::Warning,
                &format!("Do you want to save the changes you made to {name}?"),
                Some("Your changes will be lost if you don't save them."),
                &["Save", "Don't Save", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Ok(answer) = rx.await else { return };
                this.update_in(cx, |ws, window, cx| match answer {
                    0 => {
                        if editor.read(cx).doc.read(cx).path.is_some() && editor.update(cx, |e, cx| e.save(cx)).is_ok() {
                            ws.remove_tab(&editor, window, cx);
                        } else {
                            ws.activate_editor(&editor, window, cx);
                            ws.save_as(&SaveAs, window, cx);
                        }
                    }
                    1 => ws.remove_tab(&editor, window, cx),
                    _ => {}
                })
                .ok();
            })
            .detach();
        } else {
            self.remove_tab(&editor, window, cx);
        }
    }

    /// Closes a text tab without asking: what's per document goes to
    /// another tab of it, else the document is closed (and its previews).
    pub(super) fn remove_tab(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editors.iter().position(|t| t == editor) else { return };
        self.editors.remove(ix);
        let sibling = self.siblings(editor, cx).into_iter().next();
        if let Some(sibling) = &sibling {
            // The document stays open in another tab: it takes over.
            if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
                self.lsp.update(cx, |s, _| s.rebind(&path, sibling));
            }
            if let Some(unloaded) = editor.update(cx, |e, _| e.take_unloaded()) {
                sibling.update(cx, |e, _| e.put_unloaded(unloaded));
            }
        } else if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
            // Closed without saving: its edits go with it, so the store doesn't keep it.
            let doc = editor.read(cx).doc.clone();
            self.documents.update(cx, |s, cx| s.release(&doc, cx));
            self.lsp.update(cx, |s, _| s.did_close(&path));
            let breakpoints = editor.read(cx).breakpoints(cx);
            if !breakpoints.is_empty() {
                self.breakpoints.insert(path, breakpoints);
            }
        }
        self.auto_save_tasks.remove(&editor.entity_id());
        self.auto_save_warned.remove(&editor.entity_id());
        self.remove_item(&PaneItem::Editor(editor.clone()), window, cx);
        if sibling.is_none() {
            // A preview follows a document that's open; this one no longer is.
            for preview in self.previews_of(&editor.read(cx).doc.clone(), cx) {
                self.remove_item(&preview, window, cx);
            }
        }
    }

    /// Takes a tab out of its pane (a pane closes with its last tab, unless
    /// it's the only one) and shows the next one.
    pub(super) fn remove_item(&mut self, item: &PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        let id = item.entity_id();
        let Some((pane, _)) = self.locate(id, cx) else { return };
        self.tab_subscriptions.remove(&id);
        let was_active_pane = pane == self.active_pane;
        pane.update(cx, |p, cx| p.remove(id, cx));
        if pane.read(cx).is_empty() {
            self.remove_pane(&pane);
        }
        self.schedule_session_save(cx);
        self.sync_previewed(cx);
        if self.items(cx).is_empty() {
            self.show_find = false;
            self.find_bar.update(cx, |f, cx| f.set_target(None, cx));
            window.focus(&self.focus_handle);
        } else if was_active_pane || self.active_item(cx).is_none() {
            let pane = self.active_pane.clone();
            match pane.read(cx).is_empty() {
                true => window.focus(&self.focus_handle),
                false => {
                    let ix = pane.read(cx).active_ix();
                    self.activate_in(&pane, ix, window, cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane.clone();
        let (active, len) = (pane.read(cx).active_ix(), pane.read(cx).tabs().len());
        if len > 0 {
            self.activate_in(&pane, (active + 1) % len, window, cx);
        }
    }

    pub(super) fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane.clone();
        let (active, len) = (pane.read(cx).active_ix(), pane.read(cx).tabs().len());
        if len > 0 {
            self.activate_in(&pane, (active + len - 1) % len, window, cx);
        }
    }

    /// Ctrl+Tab: the pane's tab used before this one; pressed again with
    /// Ctrl still down, the one used before that, and so on.
    pub(super) fn recent_tab(&mut self, _: &RecentTab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_recent_tab(true, window, cx);
    }

    /// Ctrl+Shift+Tab: the same order from its other end.
    pub(super) fn recent_tab_back(&mut self, _: &RecentTabBack, window: &mut Window, cx: &mut Context<Self>) {
        self.step_recent_tab(false, window, cx);
    }

    fn step_recent_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane.clone();
        let tabs = pane.read(cx).tabs().to_vec();
        let mut switch = match self.tab_switch.take() {
            Some(switch) if switch.pane == pane.entity_id() => switch,
            _ => {
                let paths: Vec<Option<PathBuf>> = tabs.iter().map(|t| t.path(cx)).collect();
                let order = tabs_by_use(&paths, pane.read(cx).active_ix(), &self.recent);
                let order = order.into_iter().map(|ix| tabs[ix].clone()).collect();
                TabSwitch { pane: pane.entity_id(), order, at: 0, recent: self.recent.clone() }
            }
        };
        let len = switch.order.len();
        if len < 2 {
            return;
        }
        switch.at = if forward { (switch.at + 1) % len } else { (switch.at + len - 1) % len };
        let Some(ix) = tabs.iter().position(|t| *t == switch.order[switch.at]) else { return };
        // The tabs passed on the way keep their places in `recent`.
        self.recent = switch.recent.clone();
        self.activate_in(&pane, ix, window, cx);
        // Let go of the keys and the next press starts from the tab it ended on.
        if window.modifiers().modified() {
            self.tab_switch = Some(switch);
        }
    }
}
