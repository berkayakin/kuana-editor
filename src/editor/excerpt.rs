//! An editor showing an excerpt of its document: a run of whole lines,
//! the way a multibuffer (`crate::multibuffer`) lays out places from many
//! files. It's an ordinary editor on the file's own document (highlighting,
//! problems, Git marks, language servers and undo are the file's), whose
//! other lines take no rows: the wrap map hides them like folds, without a
//! fold's placeholder. Its height is its rows; the view around it scrolls,
//! so it never scrolls up or down itself, and it tells the view when its
//! carets move (to keep the caret in sight), when its rows change (to
//! measure it again) and when ↑/↓ go past its edge (to the next excerpt).

use std::ops::{Range, RangeInclusive};

use gpui::{App, Context, Entity, Pixels, Window, px};

use super::Editor;
use crate::document::{Document, Selection};

/// The lines an excerpt shows: where its first and last lines start. Both
/// follow edits like a caret before inserted text, so typing on the last
/// line stays in the excerpt.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Excerpt {
    first: usize,
    last: usize,
    /// The rows its height was last drawn for (the view measures it again
    /// when they change).
    pub(super) rows_drawn: usize,
    /// The editor's own edit is under way: carets it puts past the edges
    /// (Enter on the last line) grow the excerpt instead of being held in.
    pub(super) growing: bool,
}

pub enum ExcerptEvent {
    /// ↑/↓ went past the first or last row (`up`), with the caret's goal x.
    Leave { up: bool, x: Pixels },
    /// The carets moved: the view keeps the primary one in sight.
    CaretMoved,
    /// Its rows changed: the view measures it again.
    Resized,
}

impl gpui::EventEmitter<ExcerptEvent> for Editor {}

impl Excerpt {
    fn new(doc: &Document, rows: RangeInclusive<usize>) -> Self {
        let last_row = doc.line_count().saturating_sub(1);
        let (first, last) = ((*rows.start()).min(last_row), (*rows.end()).min(last_row));
        let last = last.max(first);
        Excerpt { first: doc.line_start(first), last: doc.line_start(last), rows_drawn: last - first + 1, growing: false }
    }

    /// Its lines, by row.
    fn rows(&self, doc: &Document) -> RangeInclusive<usize> {
        doc.row_of(self.first)..=doc.row_of(self.last)
    }

    /// The text it shows: from its first line's start to its last line's end.
    fn span(&self, doc: &Document) -> Range<usize> {
        self.first..doc.line_end(doc.row_of(self.last))
    }
}

impl Editor {
    /// An editor of `doc` showing lines `rows` (0-based, inclusive), with
    /// the caret at the start of the first.
    pub fn excerpt_of(doc: Entity<Document>, rows: RangeInclusive<usize>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::blank_in(doc, cx);
        this.apply_settings(cx);
        this.detect_external_linter(cx);
        let excerpt = Excerpt::new(this.doc.read(cx), rows);
        this.selections = vec![Selection::cursor(excerpt.first)];
        this.selections_generation += 1;
        this.excerpt = Some(excerpt);
        this
    }

    pub fn is_excerpt(&self) -> bool {
        self.excerpt.is_some()
    }

    /// The lines shown (0-based, inclusive); None for a whole-file editor.
    pub fn excerpt_rows(&self, cx: &App) -> Option<RangeInclusive<usize>> {
        Some(self.excerpt.as_ref()?.rows(self.doc.read(cx)))
    }

    /// Shows lines `rows` instead (more context, or two excerpts merged).
    pub fn set_excerpt_rows(&mut self, rows: RangeInclusive<usize>, cx: &mut Context<Self>) {
        let Some(old) = self.excerpt.as_ref() else { return };
        let mut excerpt = Excerpt::new(self.doc.read(cx), rows);
        excerpt.rows_drawn = old.rows_drawn;
        self.excerpt = Some(excerpt);
        // Carets it no longer shows come in; the others stay (and the view
        // isn't told they moved).
        let mut selections = self.selections.clone();
        self.hold_in_excerpt(&mut selections, cx);
        if selections != self.selections {
            self.set_selections(selections, self.primary, cx);
        }
        cx.notify();
    }

    /// The lines it hides, as folds would (see `wrap`): everything above
    /// and below the excerpt. None for a whole-file editor.
    pub(super) fn excerpt_hidden(&self, doc: &Document) -> Option<Vec<Range<usize>>> {
        let excerpt = self.excerpt.as_ref()?;
        let rows = excerpt.rows(doc);
        let mut hidden = Vec::with_capacity(2);
        if *rows.start() > 0 {
            hidden.push(0..doc.line_end(rows.start() - 1));
        }
        if rows.end() + 1 < doc.line_count() {
            hidden.push(doc.line_start(rows.end() + 1)..doc.len());
        }
        Some(hidden)
    }

    /// Moves the excerpt's lines across the document's edits since `version`.
    pub(super) fn follow_excerpt(&mut self, version: u64, cx: &App) {
        let doc = self.doc.read(cx);
        let Some(excerpt) = self.excerpt.as_mut() else { return };
        let Some(patches) = doc.patches_since(version) else {
            // Too many edits to replay (a reload as one replacement): keep the rows.
            let rows = excerpt.rows(doc);
            let last = doc.line_count().saturating_sub(1);
            excerpt.first = doc.line_start((*rows.start()).min(last));
            excerpt.last = doc.line_start((*rows.end()).min(last));
            return;
        };
        let patches: Vec<&crate::document::Patch> = patches.collect();
        let map = |offset: usize| patches.iter().fold(offset, |offset, patch| patch.map(offset));
        let (first, last) = (map(excerpt.first), map(excerpt.last));
        // Back to line starts (an edit can join a line into the one above).
        excerpt.first = doc.line_start(doc.row_of(first));
        excerpt.last = doc.line_start(doc.row_of(last)).max(excerpt.first);
        // The places it marks (a usage, a match) move with the text; one an
        // edit emptied goes.
        if !self.search_matches.ranges.is_empty() {
            let ranges: Vec<Range<usize>> = self.search_matches.ranges.iter().map(|r| map(r.start)..map(r.end)).filter(|r| r.end > r.start).collect();
            self.search_matches = crate::search::SearchMatches::new(ranges, doc.rope());
        }
    }

    /// Keeps carets in the excerpt; ones the editor's own edit put just past
    /// it (a new line at its end) grow it instead.
    pub(super) fn hold_in_excerpt(&mut self, selections: &mut [Selection], cx: &App) {
        let doc = self.doc.read(cx);
        let Some(excerpt) = self.excerpt.as_mut() else { return };
        if std::mem::take(&mut excerpt.growing) {
            let (lo, hi) = selections.iter().fold((usize::MAX, 0), |(lo, hi), s| (lo.min(s.range().start), hi.max(s.range().end)));
            if lo < excerpt.first {
                excerpt.first = doc.line_start(doc.row_of(lo));
            }
            if hi > doc.line_end(doc.row_of(excerpt.last)) {
                excerpt.last = doc.line_start(doc.row_of(hi));
            }
        }
        let span = excerpt.span(doc);
        for s in selections {
            s.anchor = s.anchor.clamp(span.start, span.end);
            s.head = s.head.clamp(span.start, span.end);
        }
    }

    /// The excerpt's height: its rows, as drawn last (before the first
    /// frame, its lines).
    pub fn excerpt_height(&self, cx: &App) -> Pixels {
        let rows = self.excerpt.as_ref().map_or(1, |e| e.rows_drawn.max(1));
        self.line_height(cx) * rows as f32
    }

    /// Puts a single caret at `offset` (held in the excerpt).
    pub fn set_caret(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.set_selections(vec![Selection::cursor(offset)], 0, cx);
    }

    /// Where it was drawn last (None before its first frame).
    pub fn shown_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
        self.layout.as_ref().map(|l| l.bounds)
    }

    pub fn excerpt_line_height(&self, cx: &App) -> Pixels {
        self.line_height(cx)
    }

    /// Where the primary caret's row is, from the editor's top: (top, bottom).
    pub fn caret_span(&self, cx: &App) -> (Pixels, Pixels) {
        let line_height = self.line_height(cx);
        let row = self.display_row_of(self.primary_selection().head, cx) as f32;
        (line_height * row, line_height * (row + 1.))
    }

    /// Comes in from the excerpt above (`from_above`) or below: the caret
    /// goes to the first or last row, at `x`.
    pub fn enter_excerpt(&mut self, from_above: bool, x: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_wrap(cx);
        let rows = self.display_row_count(cx);
        let target = if from_above { 0 } else { rows.saturating_sub(1) };
        // Not onto a block row (a code lens above the first line).
        let mut drow = target;
        while self.wrap.as_ref().is_some_and(|w| w.block_at(drow).is_some()) && drow + 1 < rows && from_above {
            drow += 1;
        }
        let offset = self.offset_for_display_row_x(drow, x.max(px(0.)), window, cx);
        self.set_selections(vec![Selection::cursor(offset)], 0, cx);
        self.goal_x = Some(x);
        cx.notify();
    }

    /// ↑/↓ past the excerpt's first or last row: the view moves on.
    pub(super) fn leaves_excerpt(&mut self, rows: isize, goal: Pixels, cx: &mut Context<Self>) -> bool {
        if self.excerpt.is_none() || self.selections.len() != 1 || !self.selections[0].is_empty() {
            return false;
        }
        let target = self.display_row_of(self.selections[0].head, cx) as isize + rows;
        let past = if rows < 0 { target < 0 } else { target >= self.display_row_count(cx) as isize };
        if past {
            self.goal_x = Some(goal);
            cx.emit(ExcerptEvent::Leave { up: rows < 0, x: goal });
        }
        past
    }

    /// After a frame's layout: the view measures it again when its rows changed.
    pub(super) fn note_excerpt_rows(&mut self, cx: &mut Context<Self>) {
        let rows = self.display_row_count(cx);
        let Some(excerpt) = self.excerpt.as_mut() else { return };
        // An edit's carets were placed by now; later moves are held in.
        excerpt.growing = false;
        if excerpt.rows_drawn != rows {
            excerpt.rows_drawn = rows;
            // Not while it's being drawn: right after.
            let this = cx.entity();
            cx.defer(move |cx| this.update(cx, |_, cx| cx.emit(ExcerptEvent::Resized)));
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;
    use crate::document::EditKind;

    fn doc(text: &str, cx: &mut TestAppContext) -> Entity<Document> {
        cx.new(|_| Document::from_text(text.into(), None))
    }

    #[gpui::test]
    fn excerpts_follow_edits_and_hold_carets(cx: &mut TestAppContext) {
        let doc = doc("a\nb\nc\nd\ne\nf\n", cx);
        let editor = cx.new(|cx| Editor::excerpt_of(doc.clone(), 2..=3, cx));
        let rows = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.excerpt_rows(cx).unwrap());
        assert_eq!(rows(cx), 2..=3);
        editor.read_with(cx, |e, cx| {
            let hidden = e.excerpt_hidden(e.doc.read(cx)).unwrap();
            assert_eq!(hidden, [0..3, 8..12], "a, b and e, f, the empty last line");
        });
        // Lines added above (in another view of the file) move it down.
        doc.update(cx, |d, cx| {
            d.edit(vec![(0..0, "x\ny\n".into())], &[], EditKind::Other);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(rows(cx), 4..=5);
        // Typing at the end of its last line stays in it; a caret moved
        // past it is held at its end.
        editor.update(cx, |e, cx| {
            let end = e.doc.read(cx).line_end(5);
            e.set_selections(vec![Selection::cursor(end)], 0, cx);
            e.insert("!", cx);
            e.set_selections(vec![Selection::cursor(0)], 0, cx);
            assert_eq!(e.selections[0].head, e.doc.read(cx).line_start(4));
            e.set_selections(vec![Selection::cursor(e.doc.read(cx).len())], 0, cx);
            assert_eq!(e.selections[0].head, e.doc.read(cx).line_end(5));
            assert_eq!(e.doc.read(cx).line_text(5), "d!");
        });
        assert_eq!(rows(cx), 4..=5);
        // A new line typed at its end is in it.
        editor.update(cx, |e, cx| {
            let end = e.doc.read(cx).line_end(5);
            e.set_selections(vec![Selection::cursor(end)], 0, cx);
            e.insert("\nnew", cx);
            assert_eq!(e.doc.read(cx).line_text(6), "new");
        });
        assert_eq!(rows(cx), 4..=6);
    }

    #[gpui::test]
    fn marks_follow_edits_elsewhere(cx: &mut TestAppContext) {
        let doc = doc("one\ntwo TODO\n", cx);
        let editor = cx.new(|cx| {
            let mut e = Editor::excerpt_of(doc.clone(), 1..=1, cx);
            let matches = crate::search::SearchMatches::new(vec![8..12], e.doc.read(cx).rope());
            e.set_search_highlights(matches, None, cx);
            e
        });
        doc.update(cx, |d, cx| {
            d.edit(vec![(0..0, "zero\n".into())], &[], EditKind::Other);
            cx.notify();
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, cx| {
            let r = e.search_matches.ranges[0].clone();
            assert_eq!(&e.doc.read(cx).rope().to_string()[r], "TODO");
        });
    }
}
