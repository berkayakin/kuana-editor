//! The scrollbar's markers: problems, Git changes, search matches and carets,
//! at whole-document scale in the track (or the minimap's lane). A document
//! can have hundreds of thousands of matches or thousands of carets, so a
//! frame doesn't walk them: each kind's lines are found when what they come
//! from changes, laid out when the display rows or the track do, and kept
//! at most one per pixel row. A frame only moves them to where the track is.

use std::ops::Range;

use gpui::{App, Bounds, Hsla, Pixels, fill, point, px, size};

use super::{Editor, FrameLayout, Same, severity_color};
use crate::{
    document::Document,
    git::{Hunk, HunkKind},
    lint::Severity,
    theme::Theme,
};

/// What display rows were when markers were laid out: the wrap map's
/// generation (None without one: rows are lines) and their number.
type DisplayKey = (Option<u64>, usize);

#[derive(Default)]
pub(super) struct Markers {
    /// For (document, version, diagnostics generation): lines with
    /// problems, each once with its worst severity.
    problems: Layer<(u64, u64, u64)>,
    /// For the hunks: their rows and kinds.
    changes: Layer<Same<Vec<Hunk>>>,
    /// For the search's lines.
    matches: Layer<Same<Vec<usize>>>,
    /// For (document, version, selections generation): the carets' lines.
    carets: Layer<(u64, u64, u64)>,
}

/// One kind of marker: its lines for `source`, then laid out for a
/// `DisplayKey` and track height.
struct Layer<K> {
    source: Option<K>,
    /// Buffer rows `start..end` (a point marker's end is `start + 1`) and a
    /// kind (severity, change kind), sorted.
    rows: Vec<(Range<usize>, u8)>,
    laid_out: Option<(DisplayKey, Pixels)>,
    /// Top and height within the track, and kind.
    marks: Vec<(Pixels, Pixels, u8)>,
    /// How many times `rows` were found or `marks` laid out.
    #[cfg(test)]
    built: usize,
}

impl<K> Default for Layer<K> {
    fn default() -> Self {
        Layer {
            source: None,
            rows: Vec::new(),
            laid_out: None,
            marks: Vec::new(),
            #[cfg(test)]
            built: 0,
        }
    }
}

impl<K: PartialEq> Layer<K> {
    /// Takes `rows()` when `source` isn't the one they're for.
    fn update(&mut self, source: K, rows: impl FnOnce() -> Vec<(Range<usize>, u8)>) {
        if self.source.as_ref() != Some(&source) {
            self.rows = rows();
            self.source = Some(source);
            self.laid_out = None;
            #[cfg(test)]
            {
                self.built += 1;
            }
        }
    }
}

/// A point marker's height, and a change's least.
const MARK: f32 = 2.;

impl Editor {
    /// Pushes the markers into the scrollbar's quads, laid out in `track`.
    pub(super) fn layout_markers(&mut self, frame: &mut FrameLayout, track: Bounds<Pixels>, cx: &App) {
        let theme = Theme::get(cx);
        let display: DisplayKey = (self.wrap.as_ref().map(|w| w.generation()), self.display_row_count(cx));
        let height = track.size.height;
        let doc = self.doc.read(cx);
        let (id, version) = (doc.id(), doc.version());

        let mut markers = std::mem::take(&mut self.markers);
        markers.problems.update((id, version, doc.diagnostics_generation()), || problem_rows(doc));
        let hunks = self.hunks.clone();
        markers.changes.update(Same(hunks.clone()), || hunks.iter().map(|h| (h.new_rows.clone(), h.kind as u8)).collect());
        let rows = self.search_matches.rows.clone();
        markers.matches.update(Same(rows.clone()), || rows.iter().map(|&row| (row..row + 1, 0)).collect());
        markers.carets.update((id, version, self.selections_generation), || {
            heads_rows(doc, self.selections.iter().map(|s| s.head)).into_iter().map(|row| (row..row + 1, 0)).collect()
        });

        let first_display_row = |row: usize| self.first_display_row(row);
        // Problems: the worst one in a pixel row shows.
        markers.problems.lay_out(display, height, &first_display_row, true);
        markers.changes.lay_out_spans(display, height, &first_display_row);
        markers.matches.lay_out(display, height, &first_display_row, false);
        markers.carets.lay_out(display, height, &first_display_row, false);

        let (top, left, right) = (track.top(), track.left(), track.right());
        for &(y, h, kind) in &markers.problems.marks {
            let bounds = Bounds::new(point(right - px(5.), top + y), size(px(4.), h));
            frame.scrollbar.push(fill(bounds, severity_color(theme, severity(kind))));
        }
        for &(y, h, kind) in &markers.changes.marks {
            let color = match kind {
                k if k == HunkKind::Added as u8 => theme.git_added,
                k if k == HunkKind::Modified as u8 => theme.git_modified,
                _ => theme.git_deleted,
            };
            frame.scrollbar.push(fill(Bounds::new(point(left, top + y), size(px(2.), h)), color));
        }
        let match_color = Hsla::from(theme.search_active).alpha(0.8);
        for &(y, h, _) in &markers.matches.marks {
            frame.scrollbar.push(fill(Bounds::new(point(left + px(3.), top + y), size(px(6.), h)), match_color));
        }
        for &(y, h, _) in &markers.carets.marks {
            frame.scrollbar.push(fill(Bounds::new(point(left + px(1.), top + y), size(px(4.), h)), theme.accent));
        }
        self.markers = markers;
    }
}

impl Markers {
    /// How many times a kind's lines were found or laid out (a frame that
    /// changed nothing does neither).
    #[cfg(test)]
    pub(super) fn rebuilds(&self) -> usize {
        self.problems.built + self.changes.built + self.matches.built + self.carets.built
    }
}

impl<K> Layer<K> {
    /// Point markers, one per pixel row: with `worst`, of the lowest kind
    /// that falls in it (the worst severity), else of one kind, so with more
    /// lines than pixel rows it skips to the next pixel row by binary search.
    fn lay_out(&mut self, display: DisplayKey, height: Pixels, first_display_row: &impl Fn(usize) -> usize, worst: bool) {
        if self.laid_out == Some((display, height)) {
            return;
        }
        self.laid_out = Some((display, height));
        #[cfg(test)]
        {
            self.built += 1;
        }
        self.marks.clear();
        let total = display.1.max(1) as f32;
        let height_px = f32::from(height);
        let y_of = |row: usize| (height_px * (first_display_row(row) as f32 / total)).round();
        let mut ix = 0;
        while ix < self.rows.len() {
            let (row, kind) = (self.rows[ix].0.start, self.rows[ix].1);
            let y = y_of(row);
            match self.marks.last_mut() {
                Some((last, _, k)) if f32::from(*last) == y => *k = (*k).min(kind),
                _ => self.marks.push((px(y), px(MARK), kind)),
            }
            ix += 1;
            if !worst && self.rows.len() > height_px as usize {
                // Rows below this pixel row's: the first whose marker is lower.
                ix += self.rows[ix..].partition_point(|(r, _)| y_of(r.start) <= y);
            }
        }
    }

    /// Changes: rows `start..end` as a bar at least `MARK` tall. Touching or
    /// overlapping bars of one kind become one; a bar in the pixel row of the
    /// previous one, both as short as they get, takes its place (it would be
    /// painted over it).
    fn lay_out_spans(&mut self, display: DisplayKey, height: Pixels, first_display_row: &impl Fn(usize) -> usize) {
        if self.laid_out == Some((display, height)) {
            return;
        }
        self.laid_out = Some((display, height));
        #[cfg(test)]
        {
            self.built += 1;
        }
        self.marks.clear();
        let total = display.1.max(1) as f32;
        let height_px = f32::from(height);
        for (rows, kind) in &self.rows {
            let (d0, d1) = (first_display_row(rows.start), first_display_row(rows.end));
            let y = height_px * (d0 as f32 / total);
            let h = (height_px * (d1.saturating_sub(d0) as f32 / total)).max(MARK);
            match self.marks.last_mut() {
                Some((top, last_h, k)) if *k == *kind && y <= f32::from(*top + *last_h) => {
                    *last_h = (*last_h).max(px(y + h) - *top);
                }
                Some((top, last_h, k)) if f32::from(*top).round() == y.round() && f32::from(*last_h) <= MARK && h <= MARK => {
                    (*top, *last_h, *k) = (px(y), px(h), *kind);
                }
                _ => self.marks.push((px(y), px(h), *kind)),
            }
        }
    }
}

fn severity(kind: u8) -> Severity {
    match kind {
        0 => Severity::Error,
        1 => Severity::Warning,
        2 => Severity::Info,
        _ => Severity::Hint,
    }
}

fn severity_kind(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
        Severity::Hint => 3,
    }
}

/// Lines with problems (information and hints aren't), each once with its worst severity.
fn problem_rows(doc: &Document) -> Vec<(Range<usize>, u8)> {
    let mut rows: Vec<(Range<usize>, u8)> = Vec::new();
    let mut line = LineCursor::default();
    for d in doc.diagnostics.iter().filter(|d| d.severity.is_problem()) {
        let row = line.row_of(doc, d.range.start);
        let kind = severity_kind(d.severity);
        match rows.last_mut() {
            Some((r, k)) if r.start == row => *k = (*k).min(kind),
            _ => rows.push((row..row + 1, kind)),
        }
    }
    rows
}

/// Per line where diagnostics start: the line, its worst problem (the first
/// of the worst severity; information and hints aren't problems) and how many start on it.
/// An empty one at a line's start counts for no line, as drawn.
pub(super) fn row_problems(doc: &Document) -> Vec<(usize, Option<usize>, usize)> {
    let mut rows: Vec<(usize, Option<usize>, usize)> = Vec::new();
    let mut line = LineCursor::default();
    for (i, d) in doc.diagnostics.iter().enumerate() {
        let row = line.row_of(doc, d.range.start);
        if d.range.end <= line.span.start {
            continue;
        }
        if rows.last().is_none_or(|r| r.0 != row) {
            rows.push((row, None, 0));
        }
        if let Some((_, worst, count)) = rows.last_mut() {
            *count += 1;
            if d.severity.is_problem() && worst.is_none_or(|w| d.severity < doc.diagnostics[w].severity) {
                *worst = Some(i);
            }
        }
    }
    rows
}

/// The lines of `offsets` (ascending, as carets of sorted selections are),
/// each once, in order.
fn heads_rows(doc: &Document, offsets: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut rows: Vec<usize> = Vec::new();
    let mut line = LineCursor::default();
    for offset in offsets {
        let row = line.row_of(doc, offset);
        if rows.last() != Some(&row) {
            rows.push(row);
        }
    }
    // Laying them out skips ahead by binary search.
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// `row_of` for ascending offsets, looking a line up only when an offset
/// leaves the last one found.
#[derive(Default)]
struct LineCursor {
    row: usize,
    /// The bytes of `row`, its newline included (empty before the first).
    span: Range<usize>,
}

impl LineCursor {
    fn row_of(&mut self, doc: &Document, offset: usize) -> usize {
        if !self.span.contains(&offset) {
            self.row = doc.row_of(offset);
            self.span = doc.line_start(self.row)..doc.line_start(self.row + 1).max(doc.line_end(self.row) + 1);
        }
        self.row
    }
}
