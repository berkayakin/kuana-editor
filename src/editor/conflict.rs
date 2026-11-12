//! Merge conflicts in the editor: tinted regions, Accept Current / Incoming /
//! Both labels on each `<<<<<<<` line (and Accept Combination when the two
//! sides change different words), and the same as actions at the caret.

use std::sync::Arc;

use gpui::{App, Context, Window};

use super::{AcceptBoth, AcceptCombination, AcceptCurrent, AcceptIncoming, EditKind, Editor, EditorLspEvent};
use crate::conflict::{self, Conflict, Resolution};

impl Editor {
    /// The file's conflicts (cached per version; the Merge view and the
    /// workspace read them here too, so the text is parsed once).
    pub(crate) fn conflicts(&mut self, cx: &App) -> Arc<Vec<Conflict>> {
        let rope = self.doc.read(cx).rope().clone();
        self.conflicts.now(self.doc.read(cx).version(), move || conflict::parse(&rope.to_string()))
    }

    /// The conflicts to draw: the last ones found (see `Derived`).
    pub(super) fn shown_conflicts(&mut self, cx: &mut Context<Self>) -> Arc<Vec<Conflict>> {
        let rope = self.doc.read(cx).rope().clone();
        self.conflicts.schedule(self.doc.read(cx).version(), move || conflict::parse(&rope.to_string()), |e| &mut e.conflicts, cx);
        self.conflicts.last()
    }

    /// Replaces the conflict starting at `start` with the side(s) kept.
    pub(super) fn resolve_conflict(&mut self, start: usize, resolution: Resolution, cx: &mut Context<Self>) {
        let Some(c) = self.conflicts(cx).iter().find(|c| c.start == start).cloned() else {
            return;
        };
        let line = |row: usize| self.doc.read(cx).line_start(row);
        let mut text: String = match (resolution, &c.merged) {
            (Resolution::Combination, Some(merged)) => merged.clone(),
            (Resolution::Combination, None) => {
                cx.emit(EditorLspEvent::Message("Both sides change the same words here: pick one, or edit it".into()));
                return;
            }
            _ => resolution.kept(&c).into_iter().map(|rows| self.doc.read(cx).slice(line(rows.start)..line(rows.end))).collect(),
        };
        let end = line(c.end + 1);
        // At the end of a file without a final newline, keep it that way.
        if end == self.doc.read(cx).len() && !self.doc.read(cx).ends_with_newline() && text.ends_with('\n') {
            text.pop();
        }
        self.edit_and_remap(vec![(line(c.start)..end, text)], EditKind::Other, cx);
    }

    fn resolve_at_caret(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        match self.conflicts(cx).iter().find(|c| c.rows().contains(&row)) {
            Some(c) => {
                let start = c.start;
                self.resolve_conflict(start, resolution, cx);
            }
            None => cx.emit(EditorLspEvent::Message("No merge conflict at the caret".into())),
        }
    }

    pub(super) fn accept_current(&mut self, _: &AcceptCurrent, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Current, cx);
    }

    pub(super) fn accept_incoming(&mut self, _: &AcceptIncoming, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Incoming, cx);
    }

    pub(super) fn accept_both(&mut self, _: &AcceptBoth, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Both, cx);
    }

    pub(super) fn accept_combination(&mut self, _: &AcceptCombination, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve_at_caret(Resolution::Combination, cx);
    }
}
