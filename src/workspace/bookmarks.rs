//! Bookmarks, as in JetBrains: Toggle Bookmark on the caret's lines, with a
//! mnemonic (0–9, A–Z, one per project) and a description, Show Bookmarks
//! (a picker; ⌫ removes), Next / Previous Bookmark across files and Go to
//! Bookmark by mnemonic. Open files keep theirs in their documents (so they
//! follow edits); the workspace keeps the rest and saves them all in the
//! project's session.

use super::*;
use crate::{
    bookmark_dialog::{MnemonicDialog, MnemonicEvent},
    document::Bookmark,
    lsp::protocol::{LspRange, Position},
    picker::BookmarkRow,
};

/// Go to Bookmark: jumps to the bookmark with this mnemonic (⌃0–9).
#[derive(Clone, PartialEq, Debug, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = workspace)]
#[serde(deny_unknown_fields)]
pub struct GoToBookmark {
    pub mnemonic: char,
}

impl Workspace {
    /// Every bookmark: open files' (which move with edits) and the rest.
    pub(super) fn all_bookmarks(&self, cx: &App) -> HashMap<PathBuf, Vec<Bookmark>> {
        let mut all = self.bookmarks.clone();
        for editor in &self.editors {
            let e = editor.read(cx);
            // A tab not read yet has its file's bookmarks in `self.bookmarks`.
            if let Some(path) = e.doc.read(cx).path.clone().filter(|_| e.is_loaded()) {
                let bookmarks = e.bookmarks(cx);
                if bookmarks.is_empty() {
                    all.remove(&path);
                } else {
                    all.insert(path, bookmarks);
                }
            }
        }
        all
    }

    /// Every bookmark in the order Next Bookmark walks them: by file, then line.
    fn bookmark_list(&self, cx: &App) -> Vec<(PathBuf, Bookmark)> {
        let mut list: Vec<(PathBuf, Bookmark)> =
            self.all_bookmarks(cx).into_iter().flat_map(|(path, marks)| marks.into_iter().map(move |b| (path.clone(), b))).collect();
        list.sort_by(|(a, x), (b, y)| a.cmp(b).then(x.row.cmp(&y.row)));
        list
    }

    pub(super) fn bookmarks_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_session_save(cx);
        self.refresh_bookmarks_picker(cx);
    }

    /// The editor and row the bookmark actions work on: the caret's line in
    /// the current tab, when it can have bookmarks.
    fn bookmark_target(&self, cx: &App) -> Option<(Entity<Editor>, usize)> {
        let editor = self.active_editor(cx)?;
        let e = editor.read(cx);
        (e.is_loaded() && e.can_bookmark(cx)).then(|| (editor.clone(), e.caret_row(cx)))
    }

    /// Toggle Bookmark: on every caret's line; when they all have one, it
    /// removes them, otherwise it adds the missing ones.
    pub(super) fn toggle_bookmark(&mut self, _: &ToggleBookmark, _: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, _)) = self.bookmark_target(cx) else { return };
        let rows = editor.read(cx).caret_rows(cx);
        let all = rows.iter().all(|&row| editor.read(cx).bookmark(row, cx).is_some());
        editor.update(cx, |e, cx| {
            for row in rows {
                if all {
                    e.remove_bookmark(row, cx);
                } else if e.bookmark(row, cx).is_none() {
                    e.set_bookmark(Bookmark::at(row), cx);
                }
            }
        });
    }

    pub(super) fn toggle_bookmark_with_mnemonic(&mut self, _: &ToggleBookmarkWithMnemonic, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, row)) = self.bookmark_target(cx) else { return };
        self.ask_mnemonic(editor, row, window, cx);
    }

    pub(super) fn edit_bookmark_description(&mut self, _: &EditBookmarkDescription, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, row)) = self.bookmark_target(cx) else { return };
        self.edit_bookmark(editor, row, window, cx);
    }

    /// Asks for the mnemonic of `row`'s bookmark (adding one if it has none).
    pub(super) fn ask_mnemonic(&mut self, editor: Entity<Editor>, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { return };
        let current = editor.read(cx).bookmark(row, cx);
        let used: HashMap<char, String> = self
            .bookmark_list(cx)
            .into_iter()
            .filter(|(p, b)| !(p == &path && b.row == row))
            .filter_map(|(p, b)| Some((b.mnemonic?, format!("{}:{}", self.location_label(&p), b.row + 1))))
            .collect();
        self.close_modal(window, cx);
        let title = format!("{} Mnemonic · Line {}", if current.is_some() { "Bookmark" } else { "Add Bookmark with" }, row + 1);
        let (mnemonic, bookmarked) = (current.as_ref().and_then(|b| b.mnemonic), current.is_some());
        let dialog = cx.new(|cx| MnemonicDialog::new(title, mnemonic, bookmarked, used, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &MnemonicEvent, window, cx| {
            ws.close_modal(window, cx);
            match event {
                MnemonicEvent::Chosen(mnemonic) => ws.set_mnemonic(&editor, row, *mnemonic, cx),
                MnemonicEvent::Remove => {
                    editor.update(cx, |e, cx| e.remove_bookmark(row, cx));
                }
                MnemonicEvent::Dismissed => {}
            }
        });
        self.modals.open(Modal::Mnemonic(dialog), subscription);
        cx.notify();
    }

    /// Bookmarks `row` with `mnemonic`, taking it off any other bookmark.
    fn set_mnemonic(&mut self, editor: &Entity<Editor>, row: usize, mnemonic: Option<char>, cx: &mut Context<Self>) {
        if let Some(m) = mnemonic {
            self.release_mnemonic(m, cx);
        }
        editor.update(cx, |e, cx| {
            let bookmark = Bookmark { mnemonic, ..e.bookmark(row, cx).unwrap_or(Bookmark::at(row)) };
            e.set_bookmark(bookmark, cx);
        });
    }

    /// Takes `mnemonic` off whichever bookmark has it (one per project).
    fn release_mnemonic(&mut self, mnemonic: char, cx: &mut Context<Self>) {
        for marks in self.bookmarks.values_mut() {
            for b in marks.iter_mut().filter(|b| b.mnemonic == Some(mnemonic)) {
                b.mnemonic = None;
            }
        }
        for editor in self.editors.clone() {
            if !editor.read(cx).is_loaded() {
                continue;
            }
            let taken = editor.read(cx).bookmarks(cx).into_iter().find(|b| b.mnemonic == Some(mnemonic));
            if let Some(b) = taken {
                editor.update(cx, |e, cx| e.set_bookmark(Bookmark { mnemonic: None, ..b }, cx));
            }
        }
    }

    /// Asks for the description of `row`'s bookmark (adding one if it has none).
    pub(super) fn edit_bookmark(&mut self, editor: Entity<Editor>, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let initial = editor.read(cx).bookmark(row, cx).and_then(|b| b.description).unwrap_or_default();
        self.close_modal(window, cx);
        let title = format!("Bookmark Description · Line {}", row + 1);
        let hint = "Shown in Show Bookmarks; leave empty for none";
        let dialog = cx.new(|cx| InputDialog::new(title, Icon::Bookmark, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(text) = event {
                let description = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                editor.update(cx, |e, cx| {
                    let bookmark = Bookmark { description, ..e.bookmark(row, cx).unwrap_or(Bookmark::at(row)) };
                    e.set_bookmark(bookmark, cx);
                });
            }
            ws.close_modal(window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// The bookmark items of a line number's right-click menu.
    pub(super) fn bookmark_menu_entries(
        &self,
        editor: &Entity<Editor>,
        row: usize,
        window: &Window,
        cx: &App,
    ) -> Vec<(MenuEntry, Option<MenuAction>)> {
        if !editor.read(cx).can_bookmark(cx) {
            return Vec::new();
        }
        let call = |f: fn(&mut Workspace, Entity<Editor>, usize, &mut Window, &mut Context<Workspace>)| {
            let editor = editor.clone();
            Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                f(ws, editor.clone(), row, window, cx)
            })))
        };
        let toggle = binding_label(&ToggleBookmark, window);
        let mnemonic = binding_label(&ToggleBookmarkWithMnemonic, window);
        match editor.read(cx).bookmark(row, cx) {
            Some(_) => vec![
                (
                    MenuEntry::item("Remove Bookmark", Icon::Bookmark).shortcut(toggle),
                    call(|_, editor, row, _, cx| {
                        editor.update(cx, |e, cx| e.remove_bookmark(row, cx));
                    }),
                ),
                (MenuEntry::item("Bookmark Mnemonic…", Icon::Keyboard).shortcut(mnemonic), call(Self::ask_mnemonic)),
                (
                    MenuEntry::item("Edit Bookmark Description…", Icon::Pencil).shortcut(binding_label(&EditBookmarkDescription, window)),
                    call(Self::edit_bookmark),
                ),
            ],
            None => vec![
                (
                    MenuEntry::item("Add Bookmark", Icon::Bookmark).shortcut(toggle),
                    call(|_, editor, row, _, cx| {
                        editor.update(cx, |e, cx| e.set_bookmark(Bookmark::at(row), cx));
                    }),
                ),
                (MenuEntry::item("Add Bookmark with Mnemonic…", Icon::Keyboard).shortcut(mnemonic), call(Self::ask_mnemonic)),
            ],
        }
    }

    /// Next Bookmark (true) / Previous Bookmark: the one after (before) the
    /// caret, in file order, going round at the ends.
    fn step_bookmark(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.bookmark_list(cx);
        if list.is_empty() {
            self.toast(ToastKind::Info, "No bookmarks", None, cx);
            return;
        }
        let here = self.active_editor(cx).and_then(|e| {
            let e = e.read(cx);
            Some((e.doc.read(cx).path.clone()?, e.caret_row(cx)))
        });
        let ix = match &here {
            Some((path, row)) if forward => list.iter().position(|(p, b)| (p, b.row) > (path, *row)).unwrap_or(0),
            Some((path, row)) => list.iter().rposition(|(p, b)| (p, b.row) < (path, *row)).unwrap_or(list.len() - 1),
            None if forward => 0,
            None => list.len() - 1,
        };
        let (path, bookmark) = list[ix].clone();
        self.open_bookmark(path, bookmark.row, window, cx);
    }

    pub(super) fn next_bookmark(&mut self, _: &NextBookmark, window: &mut Window, cx: &mut Context<Self>) {
        self.step_bookmark(true, window, cx);
    }

    pub(super) fn previous_bookmark(&mut self, _: &PreviousBookmark, window: &mut Window, cx: &mut Context<Self>) {
        self.step_bookmark(false, window, cx);
    }

    pub(super) fn go_to_bookmark(&mut self, action: &GoToBookmark, window: &mut Window, cx: &mut Context<Self>) {
        let mnemonic = action.mnemonic.to_ascii_uppercase();
        match self.bookmark_list(cx).into_iter().find(|(_, b)| b.mnemonic == Some(mnemonic)) {
            Some((path, bookmark)) => self.open_bookmark(path, bookmark.row, window, cx),
            None => self.toast(ToastKind::Info, format!("No bookmark {mnemonic}"), None, cx),
        }
    }

    fn open_bookmark(&mut self, path: PathBuf, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !path.is_file() {
            // The file went (outside Kuana): its bookmarks go with it.
            self.bookmarks.remove(&path);
            self.schedule_session_save(cx);
            self.toast(ToastKind::Info, format!("{} no longer exists", self.location_label(&path)), None, cx);
            return;
        }
        self.open_at(path, Some(row as u32 + 1), None, window, cx);
    }

    fn remove_bookmark_at(&mut self, path: &Path, row: usize, cx: &mut Context<Self>) {
        match self.editors.iter().find(|e| e.read(cx).is_loaded() && e.read(cx).doc.read(cx).path.as_deref() == Some(path)).cloned() {
            Some(editor) => {
                editor.update(cx, |e, cx| e.remove_bookmark(row, cx));
            }
            None => {
                if let Some(marks) = self.bookmarks.get_mut(path) {
                    marks.retain(|b| b.row != row);
                    if marks.is_empty() {
                        self.bookmarks.remove(path);
                    }
                }
                self.bookmarks_changed(cx);
            }
        }
    }

    /// Show Bookmarks: every bookmark in a picker, with its line.
    pub(super) fn show_bookmarks(&mut self, _: &ShowBookmarks, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Picker(p)) = &self.modals.current
            && p.read(cx).mode() == PickerMode::Bookmarks
        {
            self.close_modal(window, cx);
            return;
        }
        self.close_modal(window, cx);
        let picker = cx.new(|cx| Picker::new(PickerMode::Bookmarks, self.root.clone(), Arc::default(), Vec::new(), Arc::default(), window, cx));
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::OpenBookmark { path, row } => {
                ws.close_modal(window, cx);
                ws.open_bookmark(path.clone(), *row, window, cx);
            }
            PickerEvent::RemoveBookmark { path, row } => ws.remove_bookmark_at(path, *row, cx),
            _ => ws.close_modal(window, cx),
        });
        self.modals.open(Modal::Picker(picker), subscription);
        self.refresh_bookmarks_picker(cx);
        cx.notify();
    }

    /// Fills an open Show Bookmarks picker with the bookmarks as they are,
    /// each with its line (read in the background for files not open).
    fn refresh_bookmarks_picker(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::Picker(picker)) = &self.modals.current else { return };
        if picker.read(cx).mode() != PickerMode::Bookmarks {
            return;
        }
        let picker = picker.downgrade();
        let list = self.bookmark_list(cx);
        let locations: Vec<Location> = list
            .iter()
            .map(|(path, b)| {
                let at = Position { line: b.row as u32, character: 0 };
                Location { path: path.clone(), range: LspRange { start: at, end: at } }
            })
            .collect();
        let read = self.location_lines(&locations, cx);
        let rows: Vec<BookmarkRow> = list
            .into_iter()
            .map(|(path, b)| BookmarkRow {
                file: self.location_label(&path),
                path,
                row: b.row,
                mnemonic: b.mnemonic,
                description: b.description,
                preview: String::new(),
            })
            .collect();
        self.bookmarks_task = Some(cx.spawn(async move |_, cx| {
            let lines = read.await;
            let mut rows = rows;
            for (row, line) in rows.iter_mut().zip(lines) {
                row.preview = line.trim().to_string();
            }
            picker.update(cx, |p, cx| p.set_bookmarks(rows, cx)).ok();
        }));
    }
}
