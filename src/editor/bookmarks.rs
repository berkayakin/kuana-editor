//! The file's bookmarks: kept in its `Document` (so they follow edits and
//! every editor of it shows them); the workspace keeps closed files' ones,
//! saves them with the session and keeps mnemonics unique in the project.

use gpui::{App, Context};

use super::{Editor, EditorMode};
use crate::document::Bookmark;

/// This file's bookmarks changed (the session saves them).
pub struct EditorBookmarksChanged;

impl gpui::EventEmitter<EditorBookmarksChanged> for Editor {}

impl Editor {
    /// Bookmarks go on files' lines, not in text fields.
    pub fn can_bookmark(&self, cx: &App) -> bool {
        self.mode == EditorMode::Full && self.doc.read(cx).path.is_some()
    }

    pub fn bookmarks(&self, cx: &App) -> Vec<Bookmark> {
        self.doc.read(cx).bookmarks()
    }

    pub fn bookmark(&self, row: usize, cx: &App) -> Option<Bookmark> {
        self.doc.read(cx).bookmark(row).cloned()
    }

    /// Restores bookmarks (a file opening, a session).
    pub fn set_bookmarks(&mut self, bookmarks: &[Bookmark], cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_bookmarks(bookmarks);
        cx.notify();
    }

    /// Adds or changes the bookmark on its row.
    pub fn set_bookmark(&mut self, bookmark: Bookmark, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_bookmark(bookmark);
        cx.emit(EditorBookmarksChanged);
        cx.notify();
    }

    pub fn remove_bookmark(&mut self, row: usize, cx: &mut Context<Self>) -> Option<Bookmark> {
        let removed = self.doc.as_mut(cx).remove_bookmark(row);
        if removed.is_some() {
            cx.emit(EditorBookmarksChanged);
            cx.notify();
        }
        removed
    }

    /// The rows the carets are on, each once, in order.
    pub fn caret_rows(&self, cx: &App) -> Vec<usize> {
        let doc = self.doc.read(cx);
        let mut rows: Vec<usize> = self.selections.iter().map(|s| doc.row_of(s.head)).collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }
}
