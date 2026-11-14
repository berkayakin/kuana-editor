//! Back / Forward through the places navigations left (`crate::nav_history`).
//! What counts as a navigation: anything the workspace runs through
//! `navigate` (Go to Declaration and other locations, results, Go to Line,
//! bookmarks, `file:line` links), a tab switch (`activate_in`), and an
//! editor's own jumps (`EditorJumped`: breadcrumbs, sticky lines, Problems).
//! Typing and moving the caret by keys or clicks don't.

use super::*;
use crate::nav_history::Place;

impl Workspace {
    /// Where the caret is in the current tab, if it's a file's text.
    pub(super) fn here(&self, cx: &App) -> Option<Place> {
        let item = self.active_item(cx).filter(|item| item.is_file())?;
        let editor = item.editor()?;
        Self::place_at(editor, editor.read(cx).primary_selection().head, cx)
    }

    fn place_at(editor: &Entity<Editor>, offset: usize, cx: &App) -> Option<Place> {
        let e = editor.read(cx);
        if !e.is_loaded() {
            return None;
        }
        let doc = e.doc.read(cx);
        let path = doc.path.clone()?;
        let offset = doc.clip(offset);
        let row = doc.row_of(offset);
        let column = offset - doc.line_start(row);
        Some(Place { path, offset, row, column, doc: e.doc.entity_id(), version: doc.version() })
    }

    /// Runs `go`, a navigation: the place it leaves can be gone back to.
    /// Tab switches on the way record nothing of their own.
    pub(super) fn navigate(&mut self, window: &mut Window, cx: &mut Context<Self>, go: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>)) {
        let from = self.here(cx);
        self.nav.pause();
        go(self, window, cx);
        self.nav.resume();
        // Inside another navigation, that one records.
        if let Some(from) = from
            && !self.nav.is_paused()
        {
            let to = self.here(cx);
            self.nav.record(from, to.as_ref());
        }
    }

    /// An editor tab moved its caret by a jump of its own.
    pub(super) fn editor_jumped(&mut self, editor: &Entity<Editor>, event: &EditorJumped, cx: &mut Context<Self>) {
        let Some(from) = Self::place_at(editor, event.from, cx) else { return };
        let to = Self::place_at(editor, editor.read(cx).primary_selection().head, cx);
        self.nav.record(from, to.as_ref());
    }

    pub(super) fn navigate_back(&mut self, _: &NavigateBack, window: &mut Window, cx: &mut Context<Self>) {
        self.step_history(true, window, cx);
    }

    pub(super) fn navigate_forward(&mut self, _: &NavigateForward, window: &mut Window, cx: &mut Context<Self>) {
        self.step_history(false, window, cx);
    }

    /// Goes to the place before (`back`) or after the current one, skipping
    /// places of files gone and the line the caret is on already.
    fn step_history(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.here(cx);
        while let Some(place) = self.nav.take(back) {
            let open = self.tab_for(&place.path, cx);
            if open.is_none() && !place.path.is_file() {
                continue;
            }
            if let (Some(here), Some(editor)) = (&here, &open) {
                let offset = Self::offset_now(&place, editor, cx);
                if Self::place_at(editor, offset, cx).is_some_and(|p| p.same_line(here)) {
                    continue;
                }
            }
            self.nav.pause();
            self.open_path(place.path.clone(), window, cx);
            if let Some(editor) = self.tab_for(&place.path, cx) {
                let offset = Self::offset_now(&place, &editor, cx);
                editor.update(cx, |e, cx| e.go_to(offset, window, cx));
                window.focus(&editor.focus_handle(cx));
            }
            self.nav.resume();
            if let Some(here) = here {
                self.nav.passed(back, here);
            }
            return;
        }
    }

    /// Where a place is in its file's text now: its offset moved through the
    /// edits since, or, for a document read again since, its line and column.
    fn offset_now(place: &Place, editor: &Entity<Editor>, cx: &App) -> usize {
        let doc_entity = &editor.read(cx).doc;
        let doc = doc_entity.read(cx);
        if doc_entity.entity_id() == place.doc
            && let Some(offset) = doc.offset_since(place.version, place.offset)
        {
            return doc.clip(offset);
        }
        let row = place.row.min(doc.line_count().saturating_sub(1));
        let len = doc.line_end(row) - doc.line_start(row);
        doc.clip(doc.line_start(row) + place.column.min(len))
    }
}
