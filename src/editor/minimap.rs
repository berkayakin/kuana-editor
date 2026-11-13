//! The minimap (`minimap` setting): the document in miniature at the editor's
//! right edge, in place of the scrollbar. Each display row is a strip of
//! blocks, one per run of non-space characters in one highlight color (VS
//! Code's minimap without characters), so it costs a quad per run instead of
//! shaped text. A document taller than the strip scrolls along with the
//! editor; the visible part is a slider to drag, and a click elsewhere
//! centers that row. The scrollbar's markers (problems, search matches, Git
//! changes, carets) stay in a lane along its right edge, at whole-document
//! scale.

use std::{collections::HashMap, ops::Range, sync::Arc};

use gpui::{App, Bounds, Context, Hsla, Pixels, Point, fill, point, px, size};

use super::{Editor, FrameLayout, SCROLLBAR_WIDTH};
use crate::{settings::Settings, syntax::Highlight, theme::Theme};

/// Height of a display row.
const ROW: f32 = 3.;
/// Height of a row's blocks; the rest is the gap to the next row.
const BLOCK: f32 = 2.;
/// Width of a column.
const COLUMN: f32 = 1.;
/// Room between the editor's text and the blocks.
const PADDING: f32 = 6.;
/// The markers' lane at the right edge.
pub(super) const LANE: f32 = 10.;
/// Highlights are computed this many display rows at a time and kept while
/// the text, its syntax tree and the rows stay the same, so scrolling reuses
/// them. Rows, not lines: a long line's rows are highlighted a chunk at a
/// time too, not whole.
const CHUNK: usize = 128;

/// Highlights per chunk of display rows, for the document version, syntax
/// tree and display rows (the wrap map's generation) in `key`.
#[derive(Default)]
pub(super) struct MinimapCache {
    key: (u64, u64, Option<u64>),
    chunks: HashMap<usize, Arc<Vec<(Range<usize>, Highlight)>>>,
}

pub(super) struct MinimapGeometry {
    pub bounds: Bounds<Pixels>,
    /// The display row at the strip's top (fractional while it scrolls).
    pub first_row: f32,
    pub slider: Bounds<Pixels>,
    /// How far the slider moves per display row the editor scrolls.
    pub per_row: f32,
}

impl Editor {
    /// Whether this editor draws a minimap: the setting, in a code editor
    /// whose file isn't too large to highlight.
    pub(super) fn shows_minimap(&self, cx: &App) -> bool {
        self.file_settings.minimap && !self.is_chromeless() && !self.doc.read(cx).large && !self.is_excerpt()
    }

    /// What the right edge takes from the text: the minimap or the scrollbar.
    pub(super) fn right_edge_width(&self, cx: &App) -> Pixels {
        if self.shows_minimap(cx) {
            px(Settings::get(cx).minimap_width())
        } else if self.is_excerpt() {
            px(0.)
        } else {
            px(SCROLLBAR_WIDTH)
        }
    }

    pub(super) fn minimap_geometry(&self, cx: &App) -> Option<MinimapGeometry> {
        if !self.shows_minimap(cx) {
            return None;
        }
        let l = self.layout.as_ref()?;
        let width = self.right_edge_width(cx);
        let bounds = Bounds::new(point(l.bounds.right() - width, l.bounds.top()), size(width, l.bounds.size.height));
        let total = self.display_row_count(cx) as f32;
        let holds = bounds.size.height / px(ROW);
        let visible = l.bounds.size.height / l.line_height;
        let scrolled = self.scroll.y / l.line_height;
        // The editor scrolls until its last row is at the top, so what it
        // can show runs a screen past the last row: the minimap counts that
        // room too, or on the last page its rows would run ahead of the
        // slider and push it off the bottom.
        let max = (total - 1.).max(1.);
        let content = total - 1. + visible;
        // Taller than the strip: it scrolls in step with the editor, top to
        // top and bottom to bottom, so the slider moves slower than its rows.
        let (first_row, per_row) = if content > holds {
            let hidden = content - holds;
            (scrolled / max * hidden, ROW * (1. - hidden / max))
        } else {
            (0., ROW)
        };
        let slider = Bounds::new(
            point(bounds.left(), bounds.top() + px((scrolled - first_row) * ROW)),
            size(bounds.size.width - px(LANE), px(visible * ROW)),
        );
        Some(MinimapGeometry { bounds, first_row, slider, per_row })
    }

    /// A press on the minimap: on the slider it grabs it; elsewhere the row
    /// under the pointer comes to the middle first, and dragging goes on from
    /// there. False when the press isn't on the minimap.
    pub(super) fn minimap_mouse_down(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let (Some(g), Some(l)) = (self.minimap_geometry(cx), self.layout.as_ref()) else {
            return false;
        };
        if !g.bounds.contains(&position) {
            return false;
        }
        if !g.slider.contains(&position) {
            let row = g.first_row + (position.y - g.bounds.top()) / px(ROW);
            let visible = l.bounds.size.height / l.line_height;
            self.scroll.y = l.line_height * (row - visible / 2.).max(0.);
            self.clamp_scroll(cx);
        }
        let top = self.minimap_geometry(cx).map_or(position.y, |g| g.slider.top());
        self.scrollbar_drag = Some(position.y - top);
        cx.notify();
        true
    }

    /// Scrolls so the slider's top, grabbed `grab` above the pointer, is at `y`.
    pub(super) fn drag_minimap_to(&mut self, y: Pixels, grab: Pixels, cx: &mut Context<Self>) {
        let (Some(g), Some(l)) = (self.minimap_geometry(cx), self.layout.as_ref()) else { return };
        if g.per_row <= 0. {
            return;
        }
        let rows = ((y - grab - g.bounds.top()) / px(g.per_row)).max(0.);
        self.scroll.y = l.line_height * rows;
        self.clamp_scroll(cx);
        cx.notify();
    }

    /// The minimap's background, blocks, search matches and slider (`fade`:
    /// the scrollbar's, so the slider brightens while scrolling or hovered).
    /// Returns the markers' lane.
    pub(super) fn layout_minimap(&mut self, frame: &mut FrameLayout, g: &MinimapGeometry, fade: f32, active: bool, cx: &mut Context<Self>) -> Bounds<Pixels> {
        let theme = Theme::get(cx).clone();
        let b = g.bounds;
        frame.minimap_bounds = Some(b);
        // Over the ends of long lines, which run under it.
        frame.minimap.push(fill(b, theme.background));

        let first = g.first_row.floor() as usize;
        let shift = (g.first_row - first as f32) * ROW;
        let last = (first + (b.size.height / px(ROW)).ceil() as usize + 1).min(self.display_row_count(cx));
        let left = b.left() + px(PADDING);
        let columns = ((b.size.width - px(PADDING + LANE)) / px(COLUMN)).max(0.) as usize;
        let tab = self.tab_size.max(1);
        let match_color = Hsla::from(theme.search_active).alpha(0.8);
        for drow in first..last {
            if self.wrap.as_ref().is_some_and(|w| w.block_at(drow).is_some()) {
                continue;
            }
            let y = b.top() + px((drow - first) as f32 * ROW - shift);
            let (line, seg) = self.line_of_display_row(drow, cx);
            let (line_start, line_end) = (self.doc.read(cx).line_start(line), self.doc.read(cx).line_end(line));
            let breaks = self.line_breaks(line);
            let (from, indent) = match seg.checked_sub(1).and_then(|s| breaks.get(s)) {
                Some(b) => (line_start + b.byte as usize, b.indent as usize),
                None => (line_start, 0),
            };
            let to = breaks.get(seg).map_or(line_end, |b| line_start + b.byte as usize).max(from);
            let highlights = self.minimap_highlights(drow, cx);
            let mut span = highlights.partition_point(|(r, _)| r.end <= from);
            // Runs of non-space characters in one color: (first column, columns, color).
            let mut runs: Vec<(usize, usize, Hsla)> = Vec::new();
            let mut col = indent;
            let mut byte = from;
            let rope = self.doc.read(cx).rope();
            for ch in rope.byte_slice(from..to).chars() {
                if col >= columns {
                    break;
                }
                if ch == '\t' {
                    col += tab - col % tab;
                } else if let Some(symbol) = super::control_symbol(ch) {
                    // Its symbol, as wide as the editor shows it and as dim.
                    let width = symbol.chars().count();
                    runs.push((col, width, theme.text_dim.into()));
                    col += width;
                } else if ch.is_whitespace() {
                    col += 1;
                } else {
                    while highlights.get(span).is_some_and(|(r, _)| r.end <= byte) {
                        span += 1;
                    }
                    let kind = highlights.get(span).filter(|(r, _)| r.start <= byte).map_or(Highlight::None, |(_, h)| *h);
                    let color = theme.highlight_color(kind);
                    match runs.last_mut() {
                        Some((start, len, c)) if *start + *len == col && *c == color => *len += 1,
                        _ => runs.push((col, 1, color)),
                    }
                    col += 1;
                }
                byte += ch.len_utf8();
            }
            for (start, len, color) in runs {
                let len = len.min(columns.saturating_sub(start));
                frame.minimap.push(fill(
                    Bounds::new(point(left + px(start as f32 * COLUMN), y), size(px(len as f32 * COLUMN), px(BLOCK))),
                    color.alpha(color.a * 0.7),
                ));
            }
            // Search matches on the row, over its blocks.
            let matches = &self.search_matches.ranges;
            let first_match = matches.partition_point(|m| m.end <= from);
            for m in matches[first_match..].iter().take_while(|m| m.start < to) {
                let (start, end) = (self.minimap_column(from, m.start.max(from), indent, cx), self.minimap_column(from, m.end.min(to), indent, cx));
                if start >= columns {
                    continue;
                }
                let width = (end.min(columns).saturating_sub(start)).max(2);
                frame.minimap.push(fill(
                    Bounds::new(point(left + px(start as f32 * COLUMN), y - px(0.5)), size(px(width as f32 * COLUMN), px(ROW))),
                    match_color,
                ));
            }
        }

        let alpha = 0.07 + if active { 0.11 } else { 0.06 } * fade;
        frame.minimap.push(fill(g.slider, Hsla::from(theme.text).alpha(alpha)).corner_radii(px(5.)));
        Bounds::from_corners(point(b.right() - px(LANE), b.top()), b.bottom_right())
    }

    /// The minimap column of `offset` on the row starting at `from`.
    fn minimap_column(&self, from: usize, offset: usize, indent: usize, cx: &App) -> usize {
        let tab = self.tab_size.max(1);
        let rope = self.doc.read(cx).rope();
        rope.byte_slice(from..offset.max(from)).chars().fold(indent, |col, ch| match ch {
            '\t' => col + tab - col % tab,
            ch => col + super::control_symbol(ch).map_or(1, |symbol| symbol.chars().count()),
        })
    }

    /// The highlights of the chunk of display rows holding `drow`: of the
    /// bytes its rows show (hidden lines left out).
    fn minimap_highlights(&mut self, drow: usize, cx: &App) -> Arc<Vec<(Range<usize>, Highlight)>> {
        let doc = self.doc.read(cx);
        let key = (doc.version(), doc.tree_version(), self.wrap.as_ref().map(|w| w.generation()));
        if self.minimap_cache.key != key {
            self.minimap_cache = MinimapCache { key, chunks: HashMap::new() };
        }
        let chunk = drow / CHUNK;
        if let Some(highlights) = self.minimap_cache.chunks.get(&chunk) {
            return highlights.clone();
        }
        let rows = chunk * CHUNK..((chunk + 1) * CHUNK).min(self.display_row_count(cx));
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for drow in rows {
            if self.wrap.as_ref().is_some_and(|w| w.block_at(drow).is_some()) {
                continue;
            }
            let (line, seg) = self.line_of_display_row(drow, cx);
            let (line_start, line_end) = (doc.line_start(line), doc.line_end(line));
            let breaks = self.line_breaks(line);
            let from = seg.checked_sub(1).and_then(|s| breaks.get(s)).map_or(line_start, |b| line_start + b.from.byte as usize);
            let to = breaks.get(seg).map_or(line_end, |b| line_start + b.byte as usize).max(from);
            match ranges.last_mut() {
                Some(r) if r.end + 1 >= from => r.end = r.end.max(to),
                _ => ranges.push(from..to),
            }
        }
        let syntax = doc.syntax();
        let highlights: Arc<Vec<_>> = Arc::new(ranges.into_iter().flat_map(|r| syntax.highlights(doc.rope(), r)).collect());
        self.minimap_cache.chunks.insert(chunk, highlights.clone());
        highlights
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;
    use crate::{document::Document, editor::LayoutSnapshot};

    /// An editor showing `lines` lines in a 600 px tall minimap, 20 px per line.
    fn editor(cx: &mut TestAppContext, lines: usize) -> gpui::Entity<Editor> {
        let text = "fn main() {}\n".repeat(lines);
        cx.new(|cx| {
            let mut e = Editor::new(Document::from_text(text, None), cx);
            e.file_settings.minimap = true;
            e.layout = Some(LayoutSnapshot {
                bounds: Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.))),
                text_left: px(60.),
                line_height: px(20.),
                char_width: px(8.),
                gutter_width: px(50.),
                rows_per_page: 28,
                wrap_width: px(600.),
            });
            e
        })
    }

    #[gpui::test]
    fn a_short_file_shows_whole_and_its_slider_follows_the_rows(cx: &mut TestAppContext) {
        let editor = editor(cx, 100);
        editor.update(cx, |e, cx| {
            e.scroll.y = px(20. * 10.);
            let g = e.minimap_geometry(cx).unwrap();
            assert_eq!(g.first_row, 0.);
            assert_eq!(g.slider.top(), px(10. * ROW));
            assert_eq!(g.slider.size.height, px(30. * ROW));
            assert_eq!(g.bounds.left(), px(800.) - px(Settings::get(cx).minimap_width()));
        });
    }

    #[gpui::test]
    fn a_long_file_scrolls_the_minimap_end_to_end(cx: &mut TestAppContext) {
        let editor = editor(cx, 5000);
        editor.update(cx, |e, cx| {
            let g = e.minimap_geometry(cx).unwrap();
            assert_eq!((g.first_row, g.slider.top()), (0., px(0.)));
            // Scrolled to the end (the last row at the top): the slider's
            // bottom is the minimap's, never past it.
            let rows = e.display_row_count(cx) as f32;
            e.scroll.y = px(20. * (rows - 1.));
            let g = e.minimap_geometry(cx).unwrap();
            assert!((g.slider.bottom() - g.bounds.bottom()).abs() < px(0.01), "{:?} vs {:?}", g.slider, g.bounds);
            // The last page in view: the slider is still whole, and over the
            // rows it shows.
            e.scroll.y = px(20. * (rows - 30.));
            let g = e.minimap_geometry(cx).unwrap();
            assert!(g.slider.bottom() <= g.bounds.bottom(), "{:?} vs {:?}", g.slider, g.bounds);
            let under = g.first_row + (g.slider.top() - g.bounds.top()) / px(ROW);
            assert!((under - (rows - 30.)).abs() < 0.01, "{under}");
            // Dragging the slider back to where it is keeps the position.
            let (top, at) = (g.slider.top(), e.scroll.y);
            e.drag_minimap_to(top + px(5.), px(5.), cx);
            assert!((e.scroll.y - at).abs() < px(0.5), "{:?} vs {:?}", e.scroll.y, at);
            e.drag_minimap_to(px(-100.), px(0.), cx);
            assert_eq!(e.scroll.y, px(0.));
        });
    }

    #[gpui::test]
    fn a_click_off_the_slider_centers_that_row(cx: &mut TestAppContext) {
        let editor = editor(cx, 1000);
        editor.update(cx, |e, cx| {
            assert!(e.minimap_mouse_down(point(px(780.), px(300.)), cx));
            let g = e.minimap_geometry(cx).unwrap();
            let middle = e.scroll.y / px(20.) + 15.;
            let clicked = g.first_row + 300. / ROW;
            // Close: the minimap itself moved with the scroll.
            assert!((middle - 100.).abs() < 1., "middle {middle}, now under the pointer {clicked}");
            assert!(e.scrollbar_drag.is_some());
            assert!(!e.minimap_mouse_down(point(px(100.), px(300.)), cx));
        });
    }
}
