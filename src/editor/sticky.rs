//! Where the caret is in the file's structure: the outline (File Structure's
//! symbols, with their ranges) cached per version, the symbol chain shown as
//! breadcrumbs, and sticky scroll, which keeps the header lines of the scopes
//! scrolled into pinned at the top of the editor.

use std::sync::Arc;

use gpui::{Bounds, Context, Pixels, Point, SharedString, TextRun, fill, point, px, size};

use super::{Editor, FrameLayout};
use crate::{syntax::Symbol, theme::Theme};

/// At most this many pinned lines (fewer in short editors).
const MAX_STICKY: usize = 4;

impl Editor {
    /// The file's symbols with their ranges, recomputed when the text changes.
    /// The outline to draw: the last one computed (see `Derived`).
    fn shown_outline(&mut self, cx: &mut Context<Self>) -> Arc<Vec<Symbol>> {
        let (language, tree, rope) = (self.doc.read(cx).language, self.doc.read(cx).syntax().tree().cloned(), self.doc.read(cx).rope().clone());
        let job = move || tree.map(|tree| crate::syntax::symbols(language, &tree, &rope)).unwrap_or_default();
        self.outline.schedule((self.doc.read(cx).tree_version(), language), job, |e| &mut e.outline, cx);
        self.outline.last()
    }

    /// The definitions around the caret, outermost first: name and where it
    /// starts. Kept for the outline and caret: the workspace draws them at
    /// every frame, and a file can have thousands of symbols.
    pub fn breadcrumbs(&mut self, cx: &mut Context<Self>) -> Vec<Crumb> {
        let head = self.primary_selection().head;
        let outline = self.shown_outline(cx);
        if let Some((o, h, crumbs)) = &self.breadcrumbs
            && Arc::ptr_eq(o, &outline)
            && *h == head
        {
            return crumbs.clone();
        }
        let doc = self.doc.read(cx);
        let crumbs: Vec<Crumb> = outline
            .iter()
            .filter(|s| s.range.start <= head && head <= s.range.end)
            .map(|s| Crumb {
                name: SharedString::from(s.name.clone()),
                offset: s.offset,
                // The color the name has in the code (a function's, a type's…).
                highlight: name_highlight(doc, s.offset, s.name.len()),
            })
            .collect();
        self.breadcrumbs = Some((outline, head, crumbs.clone()));
        crumbs
    }

    /// The innermost definition around the caret (its name and range), as the
    /// outline last read knows it.
    pub fn symbol_at_caret(&mut self, cx: &mut Context<Self>) -> Option<(String, std::ops::Range<usize>)> {
        let head = self.primary_selection().head;
        let outline = self.shown_outline(cx);
        outline.iter().filter(|s| s.range.start <= head && head <= s.range.end).min_by_key(|s| s.range.len()).map(|s| (s.name.clone(), s.range.clone()))
    }

    /// The outline's symbols as (first row, last row, offset), for the
    /// outline and version drawn: sticky scroll looks them up every frame.
    fn outline_rows(&mut self, cx: &mut Context<Self>) -> Arc<Vec<(usize, usize, usize)>> {
        let outline = self.shown_outline(cx);
        let version = self.doc.read(cx).version();
        if !self.sticky_spans.0.as_ref().is_some_and(|(o, v)| Arc::ptr_eq(o, &outline) && *v == version) {
            let doc = self.doc.read(cx);
            let rows = outline.iter().map(|s| (doc.row_of(s.offset), doc.row_of(s.range.end), s.offset)).collect();
            self.sticky_spans = (Some((outline, version)), Arc::new(rows));
        }
        self.sticky_spans.1.clone()
    }

    /// The header rows to pin when `top_row` is the first visible row: scopes
    /// that start above it and continue past it, innermost last.
    fn sticky_headers(&mut self, top_row: usize, max: usize, cx: &mut Context<Self>) -> Vec<(usize, usize)> {
        let outline = self.outline_rows(cx);
        let mut rows = Vec::new();
        // The pinned lines cover rows of their own, so look below them too.
        for _ in 0..3 {
            let top = top_row + rows.len();
            let mut next: Vec<(usize, usize)> =
                outline.iter().filter(|&&(row, end, _)| row < top && end > top).map(|&(row, _, offset)| (row, offset)).collect();
            next.dedup_by_key(|(row, _)| *row);
            if next.len() > max {
                next.drain(..next.len() - max);
            }
            if next.len() == rows.len() {
                return next;
            }
            rows = next;
        }
        rows
    }

    /// Lays out the pinned lines over the top of the text (backgrounds, text
    /// and line numbers) and remembers where they are for clicks.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn layout_sticky(
        &mut self,
        frame: &mut FrameLayout,
        bounds: Bounds<Pixels>,
        top_row: usize,
        text_left: Pixels,
        number_right: Pixels,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.sticky_rows.clear();
        let line_height = frame.line_height;
        let max = MAX_STICKY.min((bounds.size.height / line_height / 4.).floor() as usize);
        if max == 0 || self.scroll.y <= px(0.) {
            return;
        }
        let headers = self.sticky_headers(top_row, max, cx);
        if headers.is_empty() {
            return;
        }
        let theme = Theme::get(cx).clone();
        let text_system = window.text_system().clone();
        let font = self.editor_font(cx);
        let font_size = self.font_size(cx);
        let width = bounds.size.width - px(super::SCROLLBAR_WIDTH);
        for (i, &(row, offset)) in headers.iter().enumerate() {
            let y = bounds.top() + line_height * i as f32;
            let row_bounds = Bounds::new(point(bounds.left(), y), size(width, line_height));
            frame.sticky_bgs.push(fill(row_bounds, theme.background));
            self.sticky_rows.push((row_bounds, offset));

            let line_start = self.doc.read(cx).line_start(row);
            // A long line's first row, as the text shows it (and no inlay hints).
            let to = self.part_bounds(row, 0..=0, cx).1;
            let text = self.doc.read(cx).slice(line_start..line_start + to);
            let dl = super::DisplayLine::new(&text, self.tab_size);
            let highlights = self.doc.read(cx).syntax().highlights(self.doc.read(cx).rope(), line_start..line_start + text.len());
            // Highlighted like the line itself (without diagnostics).
            let mut runs = Vec::new();
            let mut at = 0;
            let push = |runs: &mut Vec<TextRun>, at: &mut usize, end: usize, color: gpui::Hsla| {
                let len = dl.to_display(end) - dl.to_display(*at);
                if len > 0 {
                    runs.push(TextRun { len, font: font.clone(), color, background_color: None, underline: None, strikethrough: None });
                }
                *at = end;
            };
            for (range, kind) in &highlights {
                let s = range.start.saturating_sub(line_start).min(text.len());
                let e = range.end.saturating_sub(line_start).min(text.len());
                if s < at {
                    continue;
                }
                push(&mut runs, &mut at, s, theme.text.into());
                push(&mut runs, &mut at, e, theme.highlight_color(*kind));
            }
            push(&mut runs, &mut at, text.len(), theme.text.into());
            if !dl.controls.is_empty() {
                runs = super::restyle_runs(runs, &dl.controls, |piece| piece.color = theme.text_dim.into());
            }
            let shaped = text_system.shape_line(SharedString::from(dl.text.clone()), font_size, &runs, None);
            frame.sticky_lines.push((point(text_left - self.scroll.x, y), shaped));

            if self.shows_line_numbers() {
                let number = SharedString::from((row + 1).to_string());
                let run = TextRun { len: number.len(), font: font.clone(), color: theme.line_number.into(), background_color: None, underline: None, strikethrough: None };
                let number_line = text_system.shape_line(number, font_size, &[run], None);
                frame.sticky_numbers.push((point(number_right - number_line.width, y), number_line));
            }
        }
        // A thin line under the pinned rows separates them from the scrolling text.
        let bottom = bounds.top() + line_height * headers.len() as f32;
        frame.sticky_bgs.push(fill(Bounds::new(point(bounds.left(), bottom - px(1.)), size(width, px(1.))), theme.border));
    }

    /// The pinned line at `position`, if any: where its definition starts.
    pub(super) fn sticky_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.sticky_rows.iter().find(|(b, _)| b.contains(&position)).map(|(_, offset)| *offset)
    }
}

/// A breadcrumb: a definition around the caret.
#[derive(Clone)]
pub struct Crumb {
    pub name: SharedString,
    /// Where its name starts, to go to.
    pub offset: usize,
    /// How the editor highlights its name, if it does.
    pub highlight: Option<crate::syntax::Highlight>,
}

/// The highlight at the start of a definition's name (its first highlighted
/// span there), as the editor draws it.
fn name_highlight(doc: &crate::document::Document, offset: usize, len: usize) -> Option<crate::syntax::Highlight> {
    let end = (offset + len.max(1)).min(doc.rope().len_bytes());
    doc.syntax()
        .highlights(doc.rope(), offset..end)
        .into_iter()
        .find(|(range, h)| range.start <= offset && offset < range.end && *h != crate::syntax::Highlight::None)
        .map(|(_, h)| h)
}
