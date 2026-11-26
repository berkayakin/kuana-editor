//! Documents changed without a tab: a language server's workspace edit or
//! Replace in Files edits files that aren't open in the document store
//! instead of opening a tab for each. They stay unsaved (undoable once
//! opened) until saved: a toast reports them, the status bar lists them
//! until then, Save All there saves them, and the session keeps them as
//! tabs when the window closes first.

use super::*;

impl Workspace {
    /// The document `path` is edited in: its tab's (read first if the tab
    /// was restored and not shown yet), else one opened in the store.
    /// `Err` is why it couldn't be read.
    pub(super) fn document_to_edit(&mut self, path: &Path, cx: &mut Context<Self>) -> Task<crate::document_store::Loaded> {
        if let Some(editor) = self.tab_for(path, cx) {
            self.load_tab_now(&editor, cx);
            return match editor.read(cx).is_loaded() {
                true => Task::ready(Ok(editor.read(cx).doc.clone())),
                false => Task::ready(Err("it couldn't be read".into())),
            };
        }
        self.documents.update(cx, |s, cx| s.open(path, cx))
    }

    /// Edits a document as one undo step: through a tab showing it (its
    /// carets follow), else in the store, where it's kept until saved.
    /// Returns whether it was a document without a tab that changed.
    pub(super) fn edit_document(&mut self, doc: &Entity<Document>, edits: Vec<(std::ops::Range<usize>, String)>, cx: &mut Context<Self>) -> bool {
        if edits.is_empty() {
            return false;
        }
        match self.editors.iter().find(|t| t.read(cx).doc == *doc).cloned() {
            Some(editor) => {
                editor.update(cx, |e, cx| e.replace_ranges(edits, cx));
                false
            }
            None => self.documents.update(cx, |s, cx| s.edit(doc, edits, cx)),
        }
    }

    /// Documents just edited without a tab: their servers hear about the
    /// new text (servers that aren't running aren't started for them), auto-save
    /// saves them, and the user hears which files changed.
    pub(super) fn edited_in_background(&mut self, docs: Vec<Entity<Document>>, cx: &mut Context<Self>) {
        if docs.is_empty() {
            return;
        }
        for doc in &docs {
            self.lsp.update(cx, |s, cx| {
                s.register_document(doc, None, cx);
                s.send_edits(doc, cx);
            });
        }
        if self.auto_save != AutoSave::Off {
            self.save_background(true, false, cx);
        }
        let names: Vec<String> = docs.iter().map(|d| d.read(cx).file_name()).collect();
        let unsaved = docs.iter().filter(|d| d.read(cx).is_dirty()).count();
        let files = match names.as_slice() {
            [one] => one.clone(),
            many => format!("{} files", many.len()),
        };
        if unsaved == 0 {
            self.toast(ToastKind::Success, format!("Changed and saved {files}"), (names.len() > 1).then(|| names.join(", ")), cx);
        } else {
            let detail = format!("{}. Not open in a tab: save them here or from the status bar.", names.join(", "));
            self.toast_with_button(ToastKind::Info, format!("Changed {files}"), Some(detail), Some(ToastButton::SaveBackground), cx);
        }
        cx.notify();
    }

    /// Saves the documents edited without a tab: in the background, or here
    /// and now (`now`: closing). `auto`: for auto-save, which leaves files
    /// changed on disk meanwhile alone (Save All doesn't).
    pub(super) fn save_background(&mut self, auto: bool, now: bool, cx: &mut Context<Self>) {
        let mut jobs = Vec::new();
        let mut failed = Vec::new();
        for doc in self.documents.read(cx).kept(cx) {
            if auto && doc.read(cx).changed_on_disk() {
                continue;
            }
            match doc.update(cx, |d, _| d.save_job()) {
                Ok(job) => jobs.push((doc.downgrade(), job)),
                Err(err) => failed.push(format!("{}: {err}", doc.read(cx).file_name())),
            }
        }
        if now {
            let written = jobs.into_iter().map(|(doc, job)| (doc, job.write())).collect();
            self.background_saved(written, failed, cx);
            return;
        }
        let writes: Vec<_> = jobs.into_iter().map(|(doc, job)| (doc, cx.background_executor().spawn(async move { job.write() }))).collect();
        cx.spawn(async move |ws, cx| {
            let mut written = Vec::with_capacity(writes.len());
            for (doc, write) in writes {
                written.push((doc, write.await));
            }
            ws.update(cx, |ws, cx| ws.background_saved(written, failed, cx)).ok();
        })
        .detach();
    }

    /// Takes in the saves of documents edited without a tab.
    fn background_saved(&mut self, written: Vec<(gpui::WeakEntity<Document>, crate::document::Written)>, mut failed: Vec<String>, cx: &mut Context<Self>) {
        let mut saved = Vec::new();
        for (doc, written) in written {
            let Some(doc) = doc.upgrade() else { continue };
            match doc.update(cx, |d, cx| {
                let saved = d.saved(written);
                // The store lets go of it once it's saved.
                cx.notify();
                saved
            }) {
                Ok(()) => {
                    let d = doc.read(cx);
                    if let Some(path) = d.path.clone() {
                        self.lsp.read(cx).did_save(&path, d.rope());
                        saved.push(path);
                    }
                }
                Err(err) => failed.push(format!("{}: {err}", doc.read(cx).file_name())),
            }
        }
        if !failed.is_empty() {
            self.toast(ToastKind::Error, "Some files couldn't be saved", Some(failed.join("\n")), cx);
        }
        self.files_written(saved, cx);
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// The status bar's count of files changed without a tab (click: the list).
    pub(super) fn render_background_documents(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let count = self.documents.read(cx).kept(cx).len();
        if count == 0 {
            return None;
        }
        let tip = "Changed in files that aren't open, not saved yet";
        Some(
            div()
                .id("background-documents")
                .flex()
                .items_center()
                .gap_1()
                .h(px(24.))
                .px_2()
                .rounded(px(9.))
                .cursor_pointer()
                .text_color(theme.text_muted)
                .hover_bg(theme.hover)
                .hover(|s| s.text_color(theme.text))
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(|ws, event: &ClickEvent, window, cx| ws.open_background_menu(event.position(), window, cx)))
                .child(icon_el(Icon::Pencil, px(12.), theme.accent.into()))
                .child(format!("{count} unsaved"))
                .into_any_element(),
        )
    }

    /// The files changed without a tab: open one, or save them all.
    pub(super) fn open_background_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries = vec![MenuEntry::Header("Changed, not open".into())];
        let mut actions = vec![None];
        for doc in self.documents.read(cx).kept(cx) {
            let Some(path) = doc.read(cx).path.clone() else { continue };
            entries.push(MenuEntry::item(self.location_label(&path), Icon::FileText));
            actions.push(Some(MenuAction::OpenFile(path)));
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::item("Save All", Icon::Save));
        actions.push(Some(MenuAction::SaveBackground));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }
}
