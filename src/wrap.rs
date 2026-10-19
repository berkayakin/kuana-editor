//! Display rows: where long buffer lines break into several rows (soft
//! wrap), which lines are hidden by folds (zero rows), and block rows: rows
//! between lines that show something else than the text (a hunk's deleted
//! lines), which carets skip.
//!
//! Positions are byte offsets into a line's [`DisplayLine`] text (tabs
//! expanded, inlay hints in place: they take room on their rows like text).
//! A break's offset starts a new row, so a cursor exactly on a break shows at
//! the start of the next row. Continuation rows keep the
//! line's indentation, like VS Code.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use gpui::{Font, LineFragment, LineWrapperHandle, Pixels, SharedString, TextSystem};
use ropey::{Rope, RopeSlice};

use crate::{
    document::{InlayHint, LineEdit},
    editor::{DisplayLine, control_symbol},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Break {
    /// Display-text byte offset where the row starts.
    pub ix: u32,
    /// The same place as a byte offset in the line's own text.
    pub byte: u32,
    /// Leading spaces drawn before the row's text.
    pub indent: u32,
    /// Where building the row's display text from the buffer starts, so a
    /// long line's rows on screen are built without the rest of it (see
    /// `DisplayLine::window`).
    pub from: Resume,
}

/// A place in a line to build its display text from: a character (`byte`
/// in the line's text), where its display starts (`ix`, before the hints at
/// it) and its column, which tab stops count from. The row starting at a
/// break can start inside what that character shows (a tab's spaces, a
/// control character's name) or inside the hints before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resume {
    pub byte: u32,
    pub ix: u32,
    pub col: u32,
}

/// What line breaks depend on; a change means measuring every line again.
/// (Edits, folds and inlay hints update the map in place.)
#[derive(Clone, PartialEq)]
pub struct WrapKey {
    /// The width to wrap at; zero without soft wrap (long lines break at a
    /// fixed column then).
    pub width: Pixels,
    pub font_family: SharedString,
    pub font_size: Pixels,
    pub tab_size: usize,
    /// Break lines at the view's width (off when only folds or long lines
    /// need the map).
    pub wrap: bool,
}

pub struct WrapMap {
    pub key: WrapKey,
    /// The document version the breaks are for.
    pub version: u64,
    /// Changes whenever display rows do (see `generation`).
    generation: u64,
    /// The folded byte ranges whose lines take no rows.
    folds: Vec<Range<usize>>,
    /// Block rows above buffer lines: (line, rows), sorted; a line one past
    /// the last means after it.
    blocks: Arc<Vec<(usize, usize)>>,
    /// Breaks per buffer line; empty for lines that fit.
    breaks: Vec<Vec<Break>>,
    /// The hints each line was measured with (see `fingerprint`), to tell
    /// which lines new hints change.
    inlays: Vec<u64>,
    /// The `Inlays::generation` the breaks are for.
    inlay_generation: u64,
    /// Display rows of each line: its block rows and its text's, none when
    /// a fold hides it.
    rows: Vec<u32>,
    /// First display row of each line (its block rows, then its text),
    /// plus the total at the end (the blocks after the last line included).
    row_starts: Vec<usize>,
    /// The lines folds hide, as disjoint sorted ranges (first and last).
    hidden: Vec<(usize, usize)>,
}

/// A line longer than this (in bytes) always breaks into rows, soft wrap or
/// not: shaping, highlighting and drawing it whole each frame is too slow
/// (minified code, one-line JSON).
pub const LONG_LINE: usize = 1024;
/// The most display columns on a row of a long line without soft wrap.
const LONG_LINE_ROW: usize = 512;

/// Generations of every map, so a new map's never is an old one's.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// The inlay hints shown in the text, which lines are measured with.
#[derive(Clone, Copy)]
pub struct Inlays<'a> {
    /// Sorted by offset; empty when hints are off.
    pub hints: &'a [InlayHint],
    /// `Document::inlay_generation`, 0 when hints are off: a change means
    /// looking for the lines whose hints changed. (Hints following edits
    /// stay on the edited lines, which are measured again anyway.)
    pub generation: u64,
}

impl<'a> Inlays<'a> {
    pub const NONE: Inlays<'static> = Inlays { hints: &[], generation: 0 };

    /// Line `row`'s start and its hints: from its start to its end, the
    /// newline excluded (as `Editor::display_line` shows them).
    pub fn of_line(&self, rope: &Rope, row: usize) -> (usize, &'a [InlayHint]) {
        let row = row.min(rope.len_lines());
        let start = rope.line_to_byte(row);
        let end = if row + 1 >= rope.len_lines() { rope.len_bytes() } else { rope.line_to_byte(row + 1) - 1 };
        let first = self.hints.partition_point(|h| h.offset < start);
        let count = self.hints[first..].partition_point(|h| h.offset <= end);
        (start, &self.hints[first..first + count])
    }

    /// Every line's `fingerprint`, walking the hints rather than the lines.
    fn fingerprints(&self, rope: &Rope) -> Vec<u64> {
        let mut out = vec![0; rope.len_lines()];
        let shown = &self.hints[..self.hints.partition_point(|h| h.offset <= rope.len_bytes())];
        for line in shown.chunk_by(|a, b| rope.byte_to_line(a.offset) == rope.byte_to_line(b.offset)) {
            let row = rope.byte_to_line(line[0].offset);
            out[row] = fingerprint(rope.line_to_byte(row), line);
        }
        out
    }
}

/// A line's hints (starting at `start`) as a number, 0 without any: what its
/// breaks depend on besides its text.
fn fingerprint(start: usize, hints: &[InlayHint]) -> u64 {
    if hints.is_empty() {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    for h in hints {
        (h.offset - start, &h.label, h.pad_left, h.pad_right).hash(&mut hasher);
    }
    hasher.finish().max(1)
}

/// Finds a line's breaks; only lines that may be wider than the view are
/// shaped.
struct Measure<'a> {
    wrapper: LineWrapperHandle,
    width: Pixels,
    char_width: Pixels,
    max_cols: usize,
    tab_size: usize,
    wrap: bool,
    inlays: Inlays<'a>,
}

impl<'a> Measure<'a> {
    fn new(key: &WrapKey, inlays: Inlays<'a>, text_system: &std::sync::Arc<TextSystem>, font: Font, char_width: Pixels) -> Self {
        Measure {
            wrapper: text_system.line_wrapper(font, key.font_size),
            width: key.width,
            char_width,
            // Lines narrower than this many columns can't wrap, which skips
            // shaping-free measurement for most lines.
            max_cols: (key.width / char_width).floor().max(1.) as usize,
            tab_size: key.tab_size,
            wrap: key.wrap,
            inlays,
        }
    }

    /// Line `row`'s breaks and the `fingerprint` of the hints they're for.
    fn line(&mut self, rope: &Rope, row: usize) -> (Vec<Break>, u64) {
        let (start, hints) = self.inlays.of_line(rope, row);
        (self.breaks(rope.line(row), start, hints), fingerprint(start, hints))
    }

    /// The breaks of `line` (newline included), which starts at `start`
    /// and has `hints`.
    fn breaks(&mut self, line: RopeSlice, start: usize, hints: &[InlayHint]) -> Vec<Break> {
        let long = line.len_bytes() > LONG_LINE;
        if !self.wrap && !long {
            return Vec::new();
        }
        // A hint can also push a tab after it to the next stop.
        let hint_cols: usize =
            hints.iter().map(|h| display_columns(h.label.chars(), 1) + h.pad_left as usize + h.pad_right as usize + self.tab_size.saturating_sub(1)).sum();
        // No character takes more columns than a tab or a control character's name (NEL).
        if self.wrap
            && ((hint_cols == 0 && line.len_chars() <= self.max_cols / self.tab_size.max(3))
                || display_columns(line.chars(), self.tab_size) + hint_cols <= self.max_cols)
        {
            return Vec::new();
        }
        // Same text as `Document::line_text`: without the newline.
        let text = line.to_string();
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let inlays: Vec<(usize, &str, bool, bool)> = hints.iter().map(|h| (h.offset - start, h.label.as_str(), h.pad_left, h.pad_right)).collect();
        let dl = DisplayLine::with_inlays(text, self.tab_size, &inlays);
        let at = |rows: Vec<(usize, u32)>| {
            let ixs: Vec<usize> = rows.iter().map(|(ix, _)| *ix).collect();
            let resumes = resumes(text, &dl, self.tab_size, &ixs);
            rows.into_iter()
                .zip(resumes)
                .map(|((ix, indent), from)| Break { ix: ix as u32, byte: dl.to_buffer(ix) as u32, indent, from })
                .collect()
        };
        if !self.wrap {
            return at(dl.text.char_indices().map(|(ix, _)| (ix, 0)).step_by(LONG_LINE_ROW).skip(1).collect());
        }
        // A hint is one piece: rows break before or after it, not inside; so
        // is a control character's name (a Control Picture is one character).
        let names = dl.controls.iter().filter(|r| dl.text[(*r).clone()].is_ascii());
        let mut pieces: Vec<Range<usize>> = dl.inlays.iter().map(|s| s.whole.clone()).chain(names.cloned()).filter(|r| !r.is_empty()).collect();
        pieces.sort_by_key(|r| r.start);
        let mut fragments = Vec::with_capacity(2 * pieces.len() + 1);
        let mut from = 0;
        for piece in pieces {
            if piece.start > from {
                fragments.push(LineFragment::text(&dl.text[from..piece.start]));
            }
            let width = self.char_width * display_columns(dl.text[piece.clone()].chars(), 1) as f32;
            fragments.push(LineFragment::element(width, piece.len()));
            from = piece.end;
        }
        if from < dl.text.len() {
            fragments.push(LineFragment::text(&dl.text[from..]));
        }
        at(self.wrapper.wrap_line(&fragments, self.width).map(|b| (b.ix, b.next_indent)).collect())
    }
}

/// Where the rows starting at display offsets `ixs` (ascending) of `text`
/// shown as `dl` resume in the buffer (see `Resume`): the character whose
/// hints or display hold their first offset.
fn resumes(text: &str, dl: &DisplayLine, tab_size: usize, ixs: &[usize]) -> Vec<Resume> {
    let mut out = Vec::with_capacity(ixs.len());
    let mut next = ixs.iter().peekable();
    let mut col = 0;
    // Hints take columns too (see `DisplayLine::window`).
    let mut hints = dl.inlays.iter().peekable();
    for (i, ch) in text.char_indices() {
        // The next character's hints start where this one's display ends.
        let end = dl.to_display(i + ch.len_utf8());
        while next.next_if(|&&ix| ix < end).is_some() {
            out.push(Resume { byte: i as u32, ix: dl.to_display(i) as u32, col: col as u32 });
        }
        while let Some(hint) = hints.next_if(|s| s.column <= i) {
            col += dl.text[hint.whole.clone()].chars().count();
        }
        col += match ch {
            '\t' => tab_size - col % tab_size,
            ch => control_symbol(ch).map_or(1, |symbol| symbol.chars().count()),
        };
    }
    // In the hints after the last character.
    let last = Resume { byte: text.len() as u32, ix: dl.to_display(text.len()) as u32, col: col as u32 };
    out.extend(next.map(|_| last));
    out
}

impl WrapMap {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: WrapKey,
        version: u64,
        folds: &[Range<usize>],
        inlays: Inlays,
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> Self {
        Self::measure(key, version, folds, inlays, rope, text_system, font, char_width, &AtomicBool::new(false)).expect("not cancelled")
    }

    /// `new`, given up (None) once `cancelled` is set: it runs in the
    /// background for a big file, and a newer width makes it moot.
    #[allow(clippy::too_many_arguments)]
    pub fn measure(
        key: WrapKey,
        version: u64,
        folds: &[Range<usize>],
        inlays: Inlays,
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
        cancelled: &AtomicBool,
    ) -> Option<Self> {
        let mut measure = Measure::new(&key, inlays, text_system, font, char_width);
        let lines = rope.len_lines();
        let mut breaks = Vec::with_capacity(lines);
        let mut fingerprints = Vec::with_capacity(lines);
        // Walking the lines (rather than looking each up) and the hints
        // alongside them; most lines can't break.
        let (mut start, mut hint) = (0, 0);
        for (row, line) in rope.lines().enumerate().take(lines) {
            if row % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let next = start + line.len_bytes();
            // As `Inlays::of_line`: up to the line's end, the newline excluded.
            let end = if row + 1 >= lines { rope.len_bytes() } else { next - 1 };
            hint += inlays.hints[hint..].partition_point(|h| h.offset < start);
            let hints = &inlays.hints[hint..hint + inlays.hints[hint..].partition_point(|h| h.offset <= end)];
            breaks.push(measure.breaks(line, start, hints));
            fingerprints.push(fingerprint(start, hints));
            start = next;
        }
        debug_assert_eq!(breaks.len(), lines);
        let mut map = WrapMap {
            key,
            version,
            generation: 0,
            folds: folds.to_vec(),
            blocks: Arc::default(),
            breaks,
            inlays: fingerprints,
            inlay_generation: inlays.generation,
            rows: Vec::new(),
            row_starts: Vec::new(),
            hidden: hidden_lines(folds, rope),
        };
        map.count_rows();
        Some(map)
    }

    /// Catches up with `edits` (from `self.version` to `version`, see
    /// `Document::line_edits_since`), `folds` and `inlays`: only the lines
    /// the edits touched, whose hints changed or that folds hid or showed
    /// are measured and counted again, and the rows of the lines after them
    /// move by as many. False when the edits don't fit this map; then build
    /// a new one.
    #[allow(clippy::too_many_arguments)]
    pub fn update<'e>(
        &mut self,
        edits: impl Iterator<Item = &'e LineEdit>,
        version: u64,
        folds: &[Range<usize>],
        inlays: Inlays,
        rope: &Rope,
        text_system: &std::sync::Arc<TextSystem>,
        font: Font,
        char_width: Pixels,
    ) -> bool {
        // Asked every frame; usually nothing changed.
        if version == self.version && self.folds == folds && inlays.generation == self.inlay_generation {
            return true;
        }
        let mut dirty = Dirty::new(self.breaks.len());
        let mut blocks = None;
        for edit in edits {
            let old = edit.row..edit.row + edit.old_rows;
            if old.end > self.breaks.len() {
                return false;
            }
            self.breaks.splice(old.clone(), std::iter::repeat_with(Vec::new).take(edit.new_rows));
            self.inlays.splice(old.clone(), std::iter::repeat_n(0, edit.new_rows));
            self.rows.splice(old.clone(), std::iter::repeat_n(0, edit.new_rows));
            // The edited lines' starts; the next line's (or the total) stays
            // as it was, for `recount` to move the rest from.
            self.row_starts.splice(old.clone(), std::iter::repeat_n(0, edit.new_rows));
            dirty.splice(edit);
            // Hidden lines and blocks move with their lines, so that only
            // the ones that changed count again.
            for range in &mut self.hidden {
                *range = shift_range(*range, edit);
            }
            if !self.blocks.is_empty() {
                let moved = blocks.get_or_insert_with(|| self.blocks.to_vec());
                for (line, _) in moved.iter_mut() {
                    *line = shift_line(*line, edit);
                }
            }
        }
        if self.breaks.len() != rope.len_lines() {
            return false;
        }
        if let Some(blocks) = blocks {
            self.blocks = Arc::new(blocks);
        }
        // New hints: the lines whose hints aren't the ones they were measured
        // with (edited lines are already due).
        if inlays.generation != self.inlay_generation {
            for (row, now) in inlays.fingerprints(rope).into_iter().enumerate() {
                if self.inlays[row] != now {
                    dirty.mark(row..row + 1);
                }
            }
            self.inlay_generation = inlays.generation;
        }
        if dirty.span.is_some() {
            let mut measure = Measure::new(&self.key, inlays, text_system, font, char_width);
            for row in dirty.rows() {
                (self.breaks[row], self.inlays[row]) = measure.line(rope, row);
            }
        }
        self.version = version;
        // Folds that opened or closed show or hide lines, maybe above the
        // first edit; ones that only moved with the text change nothing.
        if self.folds != folds {
            self.folds = folds.to_vec();
            let hidden = hidden_lines(folds, rope);
            for &(first, last) in symmetric_difference(&self.hidden, &hidden) {
                dirty.mark(first..last + 1);
            }
            self.hidden = hidden;
        }
        self.recount(&dirty);
        true
    }

    /// Sets the block rows (see `blocks`); only the lines whose blocks
    /// changed count again. The same list again (asked every frame) costs
    /// nothing, and so do blocks that moved with their lines.
    pub fn set_blocks(&mut self, blocks: &Arc<Vec<(usize, usize)>>) {
        if Arc::ptr_eq(&self.blocks, blocks) {
            return;
        }
        let changed = changed_lines(&self.blocks, blocks);
        self.blocks = blocks.clone();
        if changed.is_empty() {
            return;
        }
        // One past the last line: the blocks after it.
        let mut dirty = Dirty::new(self.breaks.len() + 1);
        for line in changed {
            dirty.mark(line..line + 1);
        }
        self.recount(&dirty);
    }

    /// Changes whenever display rows may have (an edit, folds, blocks): what's
    /// computed from them (the scrollbar's markers) is kept until it does.
    /// Unique across maps.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The first display row of the blocks above `line` (where its text
    /// starts when it has none).
    pub fn block_start(&self, line: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        if line < lines { self.row_starts[line] } else { self.row_count() - self.blocks_above(lines) }
    }

    /// The block rows above buffer line `row` (`blocks` is sorted by line).
    fn blocks_above(&self, row: usize) -> usize {
        let first = self.blocks.partition_point(|&(line, _)| line < row);
        self.blocks[first..].iter().take_while(|&&(line, _)| line == row).map(|(_, n)| n).sum()
    }

    /// The block row on display row `drow`: (the line it's above, which of
    /// its block rows, counting all of that line's blocks).
    pub fn block_at(&self, drow: usize) -> Option<(usize, usize)> {
        let lines = self.row_starts.len().saturating_sub(1);
        let total = self.row_count();
        if drow >= total {
            return None;
        }
        // Blocks after the last line end the map.
        let trailing = self.blocks_above(lines);
        if drow >= total - trailing {
            return Some((lines, drow - (total - trailing)));
        }
        let line = self.line_at(drow);
        if line >= lines || self.hidden(line) {
            return None;
        }
        let start = self.row_starts[line];
        (drow < start + self.blocks_above(line)).then(|| (line, drow - start))
    }

    /// Display rows of every line.
    fn count_rows(&mut self) {
        let lines = self.breaks.len();
        self.rows = vec![0; lines];
        self.row_starts = vec![0; lines + 1];
        let mut dirty = Dirty::new(lines + 1);
        dirty.mark(0..lines + 1);
        self.recount(&dirty);
    }

    /// Counts the rows of the `dirty` lines again (one past the last: the
    /// blocks after it) and their starts; the lines after the last of them
    /// move by the difference.
    fn recount(&mut self, dirty: &Dirty) {
        self.generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        let lines = self.breaks.len();
        let Some((first, last)) = dirty.span else {
            return;
        };
        let (mut hidden, mut block) = (self.hidden.partition_point(|&(_, end)| end < first), self.blocks.partition_point(|&(line, _)| line < first));
        let mut at = if first == 0 { 0 } else { self.row_starts[first - 1] + self.rows[first - 1] as usize };
        for row in first..lines.min(last + 1) {
            // Blocks are sorted by line, and so are hidden ranges: walk them
            // alongside the lines.
            let mut above = 0;
            while let Some(&(line, n)) = self.blocks.get(block).filter(|&&(line, _)| line <= row) {
                above += if line == row { n } else { 0 };
                block += 1;
            }
            while self.hidden.get(hidden).is_some_and(|&(_, end)| end < row) {
                hidden += 1;
            }
            if dirty.lines[row] {
                let shown = self.hidden.get(hidden).is_none_or(|&(start, _)| row < start);
                self.rows[row] = if shown { (above + 1 + self.breaks[row].len()) as u32 } else { 0 };
            }
            self.row_starts[row] = at;
            at += self.rows[row] as usize;
        }
        if last >= lines {
            // Blocks after the last line count in the total.
            self.row_starts[lines] = at + self.blocks_above(lines);
        } else {
            let next = last + 1;
            let delta = at as isize - self.row_starts[next] as isize;
            if delta != 0 {
                for start in &mut self.row_starts[next..] {
                    *start = start.wrapping_add_signed(delta);
                }
            }
        }
    }

    pub fn row_count(&self) -> usize {
        *self.row_starts.last().unwrap_or(&1)
    }

    /// Whether a fold hides buffer line `row`.
    pub fn hidden(&self, row: usize) -> bool {
        if self.row_starts.len() <= row + 1 {
            return false;
        }
        matches!((self.row_starts.get(row), self.row_starts.get(row + 1)), (Some(a), Some(b)) if a == b)
    }

    pub fn breaks(&self, row: usize) -> &[Break] {
        self.breaks.get(row).map_or(&[], Vec::as_slice)
    }

    /// The first display row of buffer line `row`; one past the last line
    /// gives the total.
    pub fn first_display_row(&self, row: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        if row >= lines {
            return self.row_count();
        }
        let start = self.row_starts[row];
        if self.hidden(row) { start } else { start + self.blocks_above(row) }
    }

    /// Where buffer line `row`'s rows start: the block rows above it, if it
    /// has any, else its text.
    pub fn rows_start(&self, row: usize) -> usize {
        self.row_starts.get(row).copied().unwrap_or_else(|| self.row_count())
    }

    /// The buffer line whose rows (block rows above it included) hold
    /// display row `drow`.
    pub fn line_at(&self, drow: usize) -> usize {
        let lines = self.row_starts.len().saturating_sub(1);
        self.row_starts[..lines].partition_point(|&s| s <= drow).saturating_sub(1)
    }

    /// The buffer line and its segment index shown on display row `drow`.
    pub fn line_of(&self, drow: usize) -> (usize, usize) {
        let lines = self.row_starts.len().saturating_sub(1);
        if lines == 0 {
            return (0, 0);
        }
        let row = self.row_starts[..lines].partition_point(|&s| s <= drow).saturating_sub(1);
        let text = self.first_display_row(row);
        if drow < text {
            // A block row counts as the end of the shown line above it (the
            // top of the first line for blocks above it).
            return match (0..row).rev().find(|&r| !self.hidden(r)) {
                Some(above) => (above, self.breaks(above).len()),
                None => (row, 0),
            };
        }
        (row, (drow - text).min(self.breaks(row).len()))
    }
}

/// Which lines to measure or count again, and the span they're in (so
/// that a keystroke doesn't walk every line).
struct Dirty {
    lines: Vec<bool>,
    /// The first and last dirty line.
    span: Option<(usize, usize)>,
}

impl Dirty {
    fn new(lines: usize) -> Self {
        Dirty { lines: vec![false; lines], span: None }
    }

    fn mark(&mut self, rows: Range<usize>) {
        let rows = rows.start..rows.end.min(self.lines.len());
        if rows.is_empty() {
            return;
        }
        self.lines[rows.clone()].fill(true);
        let (first, last) = self.span.unwrap_or((rows.start, rows.end - 1));
        self.span = Some((first.min(rows.start), last.max(rows.end - 1)));
    }

    /// The lines `edit` replaces are dirty; the others move with the text.
    fn splice(&mut self, edit: &LineEdit) {
        self.lines.splice(edit.row..edit.row + edit.old_rows, std::iter::repeat_n(false, edit.new_rows));
        let end = self.lines.len();
        self.span = self.span.map(|span| shift_range(span, edit)).and_then(|(first, last)| (first < end).then(|| (first, last.min(end - 1))));
        self.mark(edit.row..edit.row + edit.new_rows);
    }

    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        let span = self.span.map_or(0..0, |(first, last)| first..last + 1);
        span.filter(|&row| self.lines[row])
    }
}

/// The lines `folds` hide (from the line a fold starts on to the one it
/// ends on), as disjoint sorted ranges.
fn hidden_lines(folds: &[Range<usize>], rope: &Rope) -> Vec<(usize, usize)> {
    let last_line = rope.len_lines().saturating_sub(1);
    let mut lines: Vec<(usize, usize)> = folds
        .iter()
        .map(|f| (rope.byte_to_line(f.start.min(rope.len_bytes())), rope.byte_to_line(f.end.min(rope.len_bytes())).min(last_line)))
        .collect();
    lines.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(lines.len());
    for (first, last) in lines {
        match out.last_mut() {
            Some(prev) if first <= prev.1 + 1 => prev.1 = prev.1.max(last),
            _ => out.push((first, last)),
        }
    }
    out
}

/// Line `line` after `edit` (a line in the edited ones goes to its first).
fn shift_line(line: usize, edit: &LineEdit) -> usize {
    if line >= edit.row + edit.old_rows {
        line + edit.new_rows - edit.old_rows
    } else {
        line.min(edit.row)
    }
}

/// Lines `first..=last` after `edit`, covering the edited ones when it
/// touches them.
fn shift_range((first, last): (usize, usize), edit: &LineEdit) -> (usize, usize) {
    let shifted = (shift_line(first, edit), shift_line(last, edit));
    if last < edit.row || first >= edit.row + edit.old_rows {
        return shifted;
    }
    (shifted.0, shifted.1.max(edit.row + edit.new_rows.saturating_sub(1)))
}

/// The ranges in one list and not the other.
fn symmetric_difference<'a>(a: &'a [(usize, usize)], b: &'a [(usize, usize)]) -> impl Iterator<Item = &'a (usize, usize)> {
    let a_only = a.iter().filter(|r| b.binary_search(r).is_err());
    a_only.chain(b.iter().filter(|r| a.binary_search(r).is_err()))
}

/// The lines whose block rows differ between `old` and `new` (sorted by
/// line).
fn changed_lines(old: &[(usize, usize)], new: &[(usize, usize)]) -> Vec<usize> {
    let total = |list: &[(usize, usize)], from: &mut usize, line: usize| {
        let mut n = 0;
        while let Some(&(_, rows)) = list.get(*from).filter(|(l, _)| *l == line) {
            n += rows;
            *from += 1;
        }
        n
    };
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < old.len() || j < new.len() {
        let line = match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) => a.0.min(b.0),
            (Some(a), None) => a.0,
            (None, Some(b)) => b.0,
            (None, None) => break,
        };
        if total(old, &mut i, line) != total(new, &mut j, line) {
            out.push(line);
        }
    }
    out
}

/// Display columns of a line's characters (tabs expanded, wide characters
/// as two, control characters as their symbols), an upper bound for whether
/// it can need wrapping.
fn display_columns(chars: impl Iterator<Item = char>, tab_size: usize) -> usize {
    let mut col = 0;
    for c in chars {
        col += match c {
            '\t' => tab_size - col % tab_size,
            '\n' => 0,
            c => match control_symbol(c) {
                Some(symbol) => display_columns(symbol.chars(), 1),
                None if (c as u32) >= 0x1100 => 2,
                None => 1,
            },
        };
    }
    col
}

/// The display-text range and indent of segment `seg` of a line with `breaks`.
pub fn segment(breaks: &[Break], seg: usize, len: usize) -> (std::ops::Range<usize>, u32) {
    let start = if seg == 0 { 0 } else { breaks[seg - 1].ix as usize };
    let end = breaks.get(seg).map_or(len, |b| b.ix as usize);
    let indent = if seg == 0 { 0 } else { breaks[seg - 1].indent };
    (start.min(len)..end.min(len).max(start.min(len)), indent)
}

/// The segment containing display column `col`.
pub fn segment_of(breaks: &[Break], col: usize) -> usize {
    breaks.partition_point(|b| b.ix as usize <= col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn edits_update_the_map_like_a_new_one(cx: &mut gpui::TestAppContext) {
        use crate::document::{Document, EditKind};
        fn inlays(doc: &Document) -> Inlays<'_> {
            Inlays { hints: doc.inlay_hints(), generation: doc.inlay_generation() }
        }
        let text_system = cx.update(|cx| cx.text_system().clone());
        let font = gpui::font("Test");
        let key = |wrap| WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap };
        let long = "word ".repeat(40);
        let text: String = (0..30).map(|i| if i % 3 == 0 { format!("{i} {long}\n") } else { format!("line {i}\n") }).collect();
        let line = |doc: &Document, row: usize| doc.line_start(row);
        // Hints at (line, column): a type in a long line, one too wide for
        // its short line, parameter names.
        let hints = |d: &Document, at: &[(usize, usize, &str)]| -> Vec<InlayHint> {
            at.iter().map(|&(row, col, label)| InlayHint { offset: line(d, row) + col, label: label.into(), pad_left: true, pad_right: false }).collect()
        };
        for wrap in [true, false] {
            let mut doc = Document::from_text(text.clone(), None);
            doc.set_inlay_hints(hints(&doc, &[(0, 1, ": Vec<String>"), (4, 6, &long), (8, 2, "name: ")]));
            let mut folds = crate::fold::Folds::default();
            folds.fold_rows(&doc, 10, 12);
            let mut map = WrapMap::new(key(wrap), doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.));
            assert_eq!(wrap, map.breaks.iter().any(|b| !b.is_empty()), "long lines wrap");
            assert_eq!(wrap, !map.breaks(4).is_empty(), "a short line wraps with a wide hint");
            let steps: Vec<Box<dyn Fn(&mut Document)>> = vec![
                // Typing in a long line, then several carets adding lines.
                Box::new(|d| drop(d.edit(vec![(line(d, 3) + 5..line(d, 3) + 5, "more ".repeat(20))], &[], EditKind::Other))),
                // New hints: the wide one moves to another line, one more.
                Box::new(|d| d.set_inlay_hints(hints(d, &[(0, 1, ": Vec<String>"), (7, 6, &long), (8, 2, "name: "), (9, 0, "p: ")]))),
                Box::new(|d| drop(d.edit(vec![(line(d, 1)..line(d, 1), "a\nb\n".into()), (line(d, 20)..line(d, 20), long.clone())], &[], EditKind::Other))),
                // Typing where a hint is (it stays after the text), deleting
                // text around one (it goes).
                Box::new(|d| {
                    let at = d.inlay_hints()[2].offset;
                    drop(d.edit(vec![(at..at, "abc".into())], &[], EditKind::Other));
                }),
                Box::new(|d| drop(d.edit(vec![(line(d, 0)..line(d, 0) + 3, String::new())], &[], EditKind::Other))),
                // Joining lines, an edit that opens the fold, undo and redo.
                Box::new(|d| drop(d.edit(vec![(line(d, 5) - 1..line(d, 7), String::new())], &[], EditKind::Other))),
                Box::new(|d| drop(d.edit(vec![(line(d, 11) + 1..line(d, 11) + 1, "x".into())], &[], EditKind::Other))),
                Box::new(|d| drop(d.undo())),
                Box::new(|d| drop(d.redo())),
                // An edit and new hints between two frames, then no hints.
                Box::new(|d| {
                    drop(d.edit(vec![(line(d, 2)..line(d, 2), long.clone())], &[], EditKind::Other));
                    d.set_inlay_hints(hints(d, &[(2, 3, &long), (15, 0, "x: ")]));
                }),
                Box::new(|d| d.set_inlay_hints(Vec::new())),
            ];
            // Block rows: above the first line, a folded one, the middle one
            // (changing), and after the last.
            let blocks = |d: &Document, n: usize| Arc::new(vec![(0, 1), (11, 2), (d.line_count() / 2, 1 + n % 3), (d.line_count(), 1)]);
            for (n, step) in steps.into_iter().enumerate() {
                // Two versions at a time, as when frames are skipped.
                let before = doc.version();
                step(&mut doc);
                step(&mut doc);
                folds.follow(&doc, before);
                let edits = doc.line_edits_since(map.version).expect("kept");
                assert!(map.update(edits, doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.)));
                map.set_blocks(&blocks(&doc, n));
                let mut fresh = WrapMap::new(key(wrap), doc.version(), folds.ranges(), inlays(&doc), doc.rope(), &text_system, font.clone(), gpui::px(7.));
                fresh.set_blocks(&blocks(&doc, n));
                assert_eq!((&map.breaks, &map.inlays, &map.rows, &map.row_starts), (&fresh.breaks, &fresh.inlays, &fresh.rows, &fresh.row_starts), "step {n}");
                assert_eq!(map.hidden, fresh.hidden, "step {n}");
            }
            // Hints switched off and on again.
            doc.set_inlay_hints(hints(&doc, &[(4, 2, &long)]));
            for shown in [Inlays::NONE, inlays(&doc)] {
                let edits = doc.line_edits_since(map.version).expect("kept");
                assert!(map.update(edits, doc.version(), folds.ranges(), shown, doc.rope(), &text_system, font.clone(), gpui::px(7.)));
                map.set_blocks(&Arc::default());
                let fresh = WrapMap::new(key(wrap), doc.version(), folds.ranges(), shown, doc.rope(), &text_system, font.clone(), gpui::px(7.));
                assert_eq!((&map.breaks, &map.inlays, &map.rows, &map.row_starts), (&fresh.breaks, &fresh.inlays, &fresh.rows, &fresh.row_starts));
            }
        }
    }

    /// A control character's name (NEL) takes its room, so a line of them
    /// wraps, and stays whole on a row.
    #[gpui::test]
    fn control_symbols_take_room_and_wrap_whole(cx: &mut gpui::TestAppContext) {
        let text_system = cx.update(|cx| cx.text_system().clone());
        let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 2, wrap: true };
        // 20 characters, 60 columns shown.
        let text = "\u{85}".repeat(20);
        let rope = Rope::from_str(&text);
        let map = WrapMap::new(key, 0, &[], Inlays::NONE, &rope, &text_system, gpui::font("Test"), gpui::px(7.));
        let breaks = map.breaks(0);
        assert!(!breaks.is_empty(), "wraps");
        let dl = DisplayLine::new(&text, 2);
        for b in breaks {
            assert!(dl.controls.iter().all(|r| b.ix as usize <= r.start || b.ix as usize >= r.end), "not inside a name: {b:?}");
            assert_eq!(dl.to_buffer(b.ix as usize), b.byte as usize);
        }
    }

    /// A hint takes room on its row and wraps as one piece; buffer columns
    /// map to the rows the display shows them on, before a hint.
    #[gpui::test]
    fn hints_take_room_on_wrapped_rows(cx: &mut gpui::TestAppContext) {
        let text_system = cx.update(|cx| cx.text_system().clone());
        let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true };
        let text = "let value = compute(first, second);";
        let rope = Rope::from_str(text);
        let label = ": Result<Vec<String>, Error>";
        let hints = [InlayHint { offset: 9, label: label.into(), pad_left: false, pad_right: false }];
        let new = |inlays| WrapMap::new(key.clone(), 0, &[], inlays, &rope, &text_system, gpui::font("Test"), gpui::px(7.));
        assert!(new(Inlays::NONE).breaks(0).is_empty(), "fits without its hint");
        let map = new(Inlays { hints: &hints, generation: 1 });
        let breaks = map.breaks(0);
        assert!(!breaks.is_empty(), "wraps with it");
        let dl = DisplayLine::with_inlays(text, 4, &[(9, label, false, false)]);
        let hint = dl.inlays[0].whole.clone();
        for b in breaks {
            assert!(b.ix as usize <= hint.start || b.ix as usize >= hint.end, "not inside the hint: {b:?}");
            assert_eq!(dl.to_buffer(b.ix as usize), b.byte as usize);
        }
        // Every buffer column shows on the row that holds its display column,
        // and a click there comes back to it.
        for col in (0..=text.len()).filter(|&c| text.is_char_boundary(c)) {
            let shown = dl.to_display(col);
            let (range, _) = segment(breaks, segment_of(breaks, shown), dl.text.len());
            assert!(range.contains(&shown) || shown == range.end, "column {col}");
            assert_eq!(dl.to_buffer(shown), col);
        }
        // The hint's column is before the hint; a click inside it goes there.
        assert_eq!(dl.to_display(9), hint.start);
        assert_eq!(dl.to_buffer(hint.start + 3), 9);
    }

    #[gpui::test]
    fn long_lines_break_without_soft_wrap(cx: &mut gpui::TestAppContext) {
        use crate::document::{Document, EditKind};
        let text_system = cx.update(|cx| cx.text_system().clone());
        let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap: false };
        // A one-line JSON file of 1300 characters, "é" (two bytes) among them.
        let json = format!("[{}é]", "1,".repeat(649));
        let mut doc = Document::from_text(format!("short\n{json}\n"), None);
        assert!(doc.has_long_lines());
        let map = WrapMap::new(key.clone(), doc.version(), &[], Inlays::NONE, doc.rope(), &text_system, gpui::font("Test"), gpui::px(7.));
        assert!(map.breaks(0).is_empty());
        let at = |n: u32| Break { ix: n, byte: n, indent: 0, from: Resume { byte: n, ix: n, col: n } };
        assert_eq!(map.breaks(1), [at(512), at(1024)]);
        assert_eq!(map.row_count(), 1 + 3 + 1);
        // Shortened below the limit, it's an ordinary line again.
        let start = doc.line_start(1);
        doc.edit(vec![(start + 10..start + 1000, String::new())], &[], EditKind::Other);
        assert!(!doc.has_long_lines());
    }

    /// A long line's rows built on their own (`DisplayLine::window` from a
    /// break's `from`) are the whole line's rows: the same text, hints and
    /// symbols, and columns map the same ways. Tabs, control characters,
    /// wide characters and hints make the display text differ from the
    /// buffer's, and breaks fall inside them.
    #[gpui::test]
    fn rows_built_alone_are_the_lines_rows(cx: &mut gpui::TestAppContext) {
        let text_system = cx.update(|cx| cx.text_system().clone());
        let line = "a\tbé\u{85}c\u{2028}世d \t".repeat(150);
        assert!(line.len() > LONG_LINE);
        let rope = Rope::from_str(&line);
        let hints: Vec<InlayHint> = line
            .char_indices()
            .map(|(i, _)| i)
            .step_by(37)
            .map(|offset| InlayHint { offset, label: ": T".into(), pad_left: offset % 2 == 0, pad_right: true })
            .collect();
        let all: Vec<(usize, &str, bool, bool)> = hints.iter().map(|h| (h.offset, h.label.as_str(), h.pad_left, h.pad_right)).collect();
        let whole = DisplayLine::with_inlays(&line, 4, &all);
        for wrap in [false, true] {
            let key = WrapKey { width: gpui::px(300.), font_family: "Test".into(), font_size: gpui::px(14.), tab_size: 4, wrap };
            let map = WrapMap::new(key, 0, &[], Inlays { hints: &hints, generation: 1 }, &rope, &text_system, gpui::font("Test"), gpui::px(7.));
            let breaks = map.breaks(0);
            assert!(breaks.len() > 2, "breaks into rows");
            for first in 0..=breaks.len() {
                for last in first..=(first + 2).min(breaks.len()) {
                    let from = if first == 0 { Resume::default() } else { breaks[first - 1].from };
                    let to = breaks.get(last).map_or(line.len(), |b| b.byte as usize);
                    let shown = from.byte as usize..=to;
                    let inlays: Vec<_> = all.iter().filter(|h| shown.contains(&h.0)).copied().collect();
                    let part = DisplayLine::window(&line[from.byte as usize..to], from, 4, &inlays);
                    assert_eq!(part.columns(), from.byte as usize..to);
                    for seg in first..=last {
                        let (range, _) = segment(breaks, seg, whole.end());
                        assert_eq!(segment(breaks, seg, part.end()).0, range, "rows {first}..={last}, row {seg}");
                        assert_eq!(part.slice(range.clone()), whole.slice(range.clone()), "row {seg}");
                        for ix in range.clone() {
                            assert_eq!(part.to_buffer(ix), whole.to_buffer(ix), "display {ix}");
                        }
                        let inside = |r: &Range<usize>| range.start <= r.start && r.end <= range.end;
                        let spans = |dl: &DisplayLine| dl.inlays.iter().map(|s| (s.whole.clone(), s.label.clone(), s.column)).filter(|s| inside(&s.0)).collect::<Vec<_>>();
                        assert_eq!(spans(&part), spans(&whole));
                        let symbols = |dl: &DisplayLine| dl.controls.iter().filter(|r| inside(r)).cloned().collect::<Vec<_>>();
                        assert_eq!(symbols(&part), symbols(&whole));
                    }
                    for col in part.columns().filter(|&c| line.is_char_boundary(c)) {
                        assert_eq!(part.to_display(col), whole.to_display(col), "column {col}");
                    }
                }
            }
        }
    }

    #[test]
    fn segments_cover_the_line() {
        let breaks = [Break { ix: 10, byte: 10, indent: 4, ..Break::default() }, Break { ix: 20, byte: 20, indent: 4, ..Break::default() }];
        assert_eq!(segment(&breaks, 0, 25), (0..10, 0));
        assert_eq!(segment(&breaks, 1, 25), (10..20, 4));
        assert_eq!(segment(&breaks, 2, 25), (20..25, 4));
        assert_eq!(segment_of(&breaks, 0), 0);
        assert_eq!(segment_of(&breaks, 9), 0);
        assert_eq!(segment_of(&breaks, 10), 1);
        assert_eq!(segment_of(&breaks, 25), 2);
        assert_eq!(segment(&[], 0, 7), (0..7, 0));
    }

    #[test]
    fn counts_display_columns() {
        let rope = Rope::from_str("\tab\n世界\n");
        assert_eq!(display_columns(rope.line(0).chars(), 4), 6);
        assert_eq!(display_columns(rope.line(0).chars(), 2), 4);
        assert_eq!(display_columns(rope.line(1).chars(), 4), 4);
    }

    #[test]
    fn maps_display_rows_to_lines() {
        let map = WrapMap {
            blocks: Arc::default(),
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: true },
            version: 0,
            generation: 0,
            folds: Vec::new(),
            breaks: vec![vec![], vec![Break { ix: 5, byte: 5, ..Break::default() }, Break { ix: 9, byte: 9, ..Break::default() }], vec![]],
            inlays: vec![0; 3],
            inlay_generation: 0,
            rows: vec![1, 3, 1],
            row_starts: vec![0, 1, 4, 5],
            hidden: Vec::new(),
        };
        assert_eq!(map.row_count(), 5);
        assert_eq!(map.line_of(0), (0, 0));
        assert_eq!(map.line_of(1), (1, 0));
        assert_eq!(map.line_of(3), (1, 2));
        assert_eq!(map.line_of(4), (2, 0));
        assert_eq!(map.line_of(99), (2, 0));
        assert_eq!(map.first_display_row(2), 4);
    }

    #[gpui::test]
    fn block_rows_sit_between_lines(cx: &mut gpui::TestAppContext) {
        let rope = Rope::from_str("a\nb\nc\n");
        let key = WrapKey { width: gpui::px(400.), font_family: "Menlo".into(), font_size: gpui::px(13.), tab_size: 4, wrap: false };
        let mut map = cx.update(|cx| WrapMap::new(key, 0, &[], Inlays::NONE, &rope, cx.text_system(), gpui::font("Menlo"), gpui::px(8.)));
        // Two rows above line 1 ("b"), one after the last line.
        map.set_blocks(&Arc::new(vec![(1, 2), (4, 1)]));
        assert_eq!(map.row_count(), 4 + 3);
        assert_eq!(map.first_display_row(1), 3);
        assert_eq!((map.block_at(1), map.block_at(2), map.block_at(3)), (Some((1, 0)), Some((1, 1)), None));
        assert_eq!(map.block_at(6), Some((4, 0)));
        // A block row maps to the line above it, for clicks and carets.
        assert_eq!(map.line_of(2), (0, 0));
        assert_eq!(map.line_of(3), (1, 0));
        map.set_blocks(&Arc::default());
        assert_eq!((map.row_count(), map.first_display_row(1)), (4, 1));
    }

    /// Lookups by binary search and the walk in `count_rows` agree with
    /// counting every row: several blocks on one line, blocks above folded
    /// lines (no rows) and after the last line.
    #[gpui::test]
    fn block_lookups_match_counting_rows(cx: &mut gpui::TestAppContext) {
        let rope = Rope::from_str(&"x\n".repeat(12));
        let key = WrapKey { width: gpui::px(0.), font_family: "Menlo".into(), font_size: gpui::px(13.), tab_size: 4, wrap: false };
        // Lines 5 and 6 folded away (the fold covers them whole).
        let fold = rope.line_to_byte(5)..rope.line_to_byte(7) - 1;
        let mut map = cx.update(|cx| WrapMap::new(key, 0, std::slice::from_ref(&fold), Inlays::NONE, &rope, cx.text_system(), gpui::font("Menlo"), gpui::px(8.)));
        let blocks = [(0, 1), (2, 2), (2, 1), (5, 3), (9, 1), (13, 2)];
        map.set_blocks(&Arc::new(blocks.to_vec()));
        // Every display row by hand: (line, Some(block row)) or (line, None)
        // for its text; one past the last line holds the blocks after it.
        let lines = rope.len_lines();
        let mut rows: Vec<(usize, Option<usize>)> = Vec::new();
        for line in (0..=lines).filter(|line| !(5..=6).contains(line)) {
            let above: usize = blocks.iter().filter(|(l, _)| *l == line).map(|(_, n)| n).sum();
            rows.extend((0..above).map(|i| (line, Some(i))));
            if line < lines {
                rows.push((line, None));
            }
        }
        assert_eq!(map.row_count(), rows.len());
        for (drow, &(line, block)) in rows.iter().enumerate() {
            assert_eq!(map.block_at(drow), block.map(|i| (line, i)), "row {drow}");
            if block.is_none() {
                assert_eq!(map.first_display_row(line), drow, "line {line}");
            }
        }
        assert_eq!(map.block_at(rows.len()), None);
    }

    #[test]
    fn folded_lines_take_no_rows() {
        // Lines 1 and 2 are hidden: row_starts repeats.
        let map = WrapMap {
            blocks: Arc::default(),
            key: WrapKey { width: gpui::px(100.), font_family: "".into(), font_size: gpui::px(14.), tab_size: 4, wrap: false },
            version: 0,
            generation: 0,
            folds: Vec::new(),
            breaks: vec![vec![], vec![], vec![], vec![]],
            inlays: vec![0; 4],
            inlay_generation: 0,
            rows: vec![1, 0, 0, 1],
            row_starts: vec![0, 1, 1, 1, 2],
            hidden: vec![(1, 2)],
        };
        assert_eq!(map.row_count(), 2);
        assert!(map.hidden(1) && map.hidden(2) && !map.hidden(0) && !map.hidden(3));
        assert_eq!(map.line_of(1), (3, 0));
        assert_eq!(map.first_display_row(3), 1);
    }
}
