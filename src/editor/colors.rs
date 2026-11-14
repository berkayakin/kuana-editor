//! Color swatches and the color picker, as VS Code's color decorators: a
//! swatch (`document::SWATCH`, an inlay drawn in the color) before each
//! color in the text, and a click on it opens a picker that rewrites the
//! color in its own notation (`crate::color::Format`) as you drag, one undo
//! step in all. The colors come from a language server with `colorProvider`
//! (`textDocument/documentColor`: CSS's, Tailwind's), else, in CSS-like
//! languages and HTML, from a scan of the text (`crate::color::find`).

use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, Context, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba, TextRun, canvas,
    div, font, linear_color_stop, linear_gradient, point, prelude::*, px,
};
use ropey::Rope;
use serde_json::{Value, json};

use super::Editor;
use crate::{
    color::{self, Color, Format},
    document::{DocColor, EditKind, SWATCH},
    lsp::{Server, protocol},
    theme::Theme,
};

/// How long after an edit colors are looked for again.
const COLOR_PAUSE: Duration = Duration::from_millis(150);
/// Languages whose text is scanned for colors (by name; extensions' too).
const SCANNED: &[&str] = &["css", "scss", "sass", "less", "stylus", "postcss", "html", "vue", "svelte", "astro"];
/// Lines longer than this aren't scanned (minified files).
const MAX_SCANNED_LINE: usize = 4000;
/// The picker's parts, in points.
const FIELD: (f32, f32) = (200., 132.);
const BAR: f32 = 12.;
/// The whole picker's height: padding, field, two bars, the footer, gaps.
const PICKER_HEIGHT: f32 = 16. + FIELD.1 + 2. * BAR + 18. + 3. * 8. + 2.;

/// The picker open on a color.
pub(super) struct ColorPicker {
    /// Where the color's text is now (it changes length as it's rewritten).
    range: Range<usize>,
    format: Format,
    hue: f32,
    saturation: f32,
    value: f32,
    alpha: f32,
    /// Editor-local top left of the swatch, and its height.
    anchor: Point<Pixels>,
    /// The undo history when it opened: everything picked is one step.
    undo_mark: usize,
    /// The part being dragged.
    dragging: Option<Part>,
    /// Where the field and bars were drawn, for the pointer.
    parts: Rc<Cell<[Bounds<Pixels>; 3]>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Field,
    Hue,
    Alpha,
}

impl ColorPicker {
    fn color(&self) -> Color {
        Color::from_hsv(self.hue, self.saturation, self.value, self.alpha)
    }
}

impl Editor {
    pub(super) fn colors_on(&self, cx: &App) -> bool {
        self.mode == super::EditorMode::Full && !self.doc.read(cx).large && self.file_settings.document_colors
    }

    fn scans_colors(&self, cx: &App) -> bool {
        let name = self.doc.read(cx).language.name().to_ascii_lowercase();
        SCANNED.contains(&name.as_str())
    }

    /// Looks for the colors again once edits pause: asks the server that
    /// finds colors, else scans the text in the background.
    pub(super) fn schedule_colors(&mut self, cx: &mut Context<Self>) {
        if !self.colors_on(cx) {
            self.color_task = None;
            return;
        }
        let delay = if self.colors_asked { COLOR_PAUSE } else { Duration::ZERO };
        self.colors_asked = true;
        self.color_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok((version, rope, server, params, scan)) = this.update(cx, |e, cx| {
                let server = e.lsp_server_with("colorProvider", cx);
                let params = e.text_document(cx).map(|doc| json!({ "textDocument": doc }));
                (e.doc.read(cx).version(), e.doc.read(cx).rope().clone(), server, params, e.scans_colors(cx))
            }) else {
                return;
            };
            let mut colors = None;
            if let (Some(server), Some(params)) = (server, params)
                && let Ok(answer) = server.request("textDocument/documentColor", params).await
            {
                colors = Some(read_document_colors(&answer, &rope, &server));
            }
            let colors = match colors {
                Some(colors) => colors,
                None if scan => cx.background_executor().spawn(async move { scan_colors(&rope) }).await,
                None => Vec::new(),
            };
            this.update(cx, |e, cx| {
                e.doc.as_mut(cx).set_colors(version, colors);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The color for a swatch drawn at line `line_start`'s buffer column.
    pub(super) fn swatch_color(&self, line_start: usize, column: usize, cx: &App) -> Option<Hsla> {
        let c = self.doc.read(cx).color_at(line_start + column)?.color;
        Some(Rgba { r: c.r, g: c.g, b: c.b, a: c.a.max(0.15) }.into())
    }

    /// A press on a swatch: the picker opens on its color (for colors it can
    /// write back). `bounds` is the swatch, in window coordinates.
    pub(super) fn open_color_picker(&mut self, start: usize, bounds: Bounds<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(origin) = self.layout.as_ref().map(|l| l.bounds.origin) else { return false };
        let Some(found) = self.doc.read(cx).color_at(start).cloned() else { return false };
        let Some(format) = found.format else { return false };
        let (hue, saturation, value) = found.color.to_hsv();
        self.close_color_picker(cx);
        self.color_picker = Some(ColorPicker {
            range: found.range.clone(),
            format,
            hue,
            saturation,
            value,
            alpha: found.color.a,
            anchor: point(bounds.left() - origin.x, bounds.bottom() - origin.y),
            undo_mark: self.doc.read(cx).undo_mark(),
            dragging: None,
            parts: Rc::new(Cell::new([Bounds::default(); 3])),
        });
        cx.notify();
        true
    }

    /// Closes the picker; what it changed is one undo step.
    pub(super) fn close_color_picker(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(picker) = self.color_picker.take() else { return false };
        self.doc.as_mut(cx).merge_undo_since(picker.undo_mark);
        cx.notify();
        true
    }

    /// The pointer at `position` in `part`: the color there, written into the text.
    fn pick(&mut self, part: Part, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(picker) = self.color_picker.as_mut() else { return };
        let bounds = picker.parts.get()[part as usize];
        if bounds.size.width <= px(0.) {
            return;
        }
        let x = ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
        let y = ((position.y - bounds.top()) / bounds.size.height).clamp(0., 1.);
        match part {
            Part::Field => (picker.saturation, picker.value) = (x, 1. - y),
            Part::Hue => picker.hue = x * 360.,
            Part::Alpha => picker.alpha = (x * 100.).round() / 100.,
        }
        let text = picker.format.write(picker.color());
        let range = picker.range.clone();
        if self.doc.read(cx).slice(range.clone()) == text {
            cx.notify();
            return;
        }
        if let Some(picker) = self.color_picker.as_mut() {
            picker.range = range.start..range.start + text.len();
        }
        self.edit_and_remap(vec![(range, text)], EditKind::Other, cx);
    }

    /// The picker under its swatch: a saturation/brightness field, hue and
    /// alpha bars, and the color as written.
    pub(super) fn render_color_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.color_picker.as_ref()?;
        let layout = self.layout.as_ref()?;
        let theme = Theme::get(cx).clone();
        let color = picker.color();
        let hsla = |c: Color| -> Hsla { Rgba { r: c.r, g: c.g, b: c.b, a: c.a }.into() };
        let opaque = Color { a: 1., ..color };
        let pure_hue = Color::from_hsv(picker.hue, 1., 1., 1.);
        let (fw, fh) = (px(FIELD.0), px(FIELD.1));
        // Notes where a part was drawn.
        let note = |part: Part, parts: Rc<Cell<[Bounds<Pixels>; 3]>>| {
            canvas(
                move |bounds, _, _| {
                    let mut all = parts.get();
                    all[part as usize] = bounds;
                    parts.set(all);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };
        let knob = |x: Pixels, y: Pixels| {
            div()
                .absolute()
                .left(x - px(6.))
                .top(y - px(6.))
                .size(px(12.))
                .rounded_full()
                .border_2()
                .border_color(gpui::white())
                .shadow(crate::ui::soft_shadow(&theme, 1))
        };
        let bar_knob = |x: Pixels| div().absolute().left(x - px(2.)).top(px(-2.)).w(px(4.)).h(px(BAR + 4.)).rounded(px(2.)).border_1().border_color(gpui::black().opacity(0.5)).bg(gpui::white());
        let white: Hsla = gpui::white();
        let black: Hsla = gpui::black();

        let field = div()
            .relative()
            .w(fw)
            .h(fh)
            .rounded(px(4.))
            .bg(hsla(pure_hue))
            .child(div().absolute().size_full().rounded(px(4.)).bg(linear_gradient(90., linear_color_stop(white, 0.), linear_color_stop(white.opacity(0.), 1.))))
            .child(div().absolute().size_full().rounded(px(4.)).bg(linear_gradient(180., linear_color_stop(black.opacity(0.), 0.), linear_color_stop(black, 1.))))
            .child(note(Part::Field, picker.parts.clone()))
            .child(knob(fw * picker.saturation, fh * (1. - picker.value)));
        let hue_bar = div()
            .relative()
            .w(fw)
            .h(px(BAR))
            .rounded(px(3.))
            .flex()
            .children((0..6).map(|i| {
                let from = hsla(Color::from_hsv(i as f32 * 60., 1., 1., 1.));
                let to = hsla(Color::from_hsv((i + 1) as f32 * 60., 1., 1., 1.));
                div().flex_1().h_full().bg(linear_gradient(90., linear_color_stop(from, 0.), linear_color_stop(to, 1.)))
            }))
            .child(note(Part::Hue, picker.parts.clone()))
            .child(bar_knob(fw * (picker.hue / 360.)));
        let alpha_bar = div()
            .relative()
            .w(fw)
            .h(px(BAR))
            .rounded(px(3.))
            .bg(theme.text_dim)
            .child(
                div()
                    .absolute()
                    .size_full()
                    .rounded(px(3.))
                    .bg(linear_gradient(90., linear_color_stop(hsla(Color { a: 0., ..opaque }), 0.), linear_color_stop(hsla(opaque), 1.))),
            )
            .child(note(Part::Alpha, picker.parts.clone()))
            .child(bar_knob(fw * picker.alpha));
        let text = picker.format.write(color);
        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(18.)).rounded(px(4.)).border_1().border_color(theme.border).bg(hsla(color)))
            .child(div().font(font(theme.mono_font.clone())).text_size(px(theme.editor_font_size - 1.)).text_color(theme.text).child(text));

        // Under the swatch, or above it when there's no room below.
        let below = picker.anchor.y + px(4. + PICKER_HEIGHT) <= layout.bounds.size.height;
        let top = if below { picker.anchor.y + px(4.) } else { (picker.anchor.y - layout.line_height - px(4. + PICKER_HEIGHT)).max(px(0.)) };
        let left = picker.anchor.x.min(layout.bounds.size.width - px(FIELD.0 + 28.)).max(px(0.));
        Some(
            div()
                .id("color-picker")
                .absolute()
                .left(left)
                .top(top)
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .bg(theme.elevated)
                .border_1()
                .border_color(Hsla::from(theme.border).alpha(0.85))
                .rounded(px(10.))
                .shadow(crate::ui::soft_shadow(&theme, 2))
                .cursor_crosshair()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|e, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let Some(picker) = e.color_picker.as_mut() else { return };
                        let parts = picker.parts.get();
                        let part = [Part::Field, Part::Hue, Part::Alpha].into_iter().find(|p| parts[*p as usize].contains(&event.position));
                        picker.dragging = part;
                        if let Some(part) = part {
                            e.pick(part, event.position, cx);
                        }
                    }),
                )
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_move(cx.listener(|e, event: &MouseMoveEvent, _, cx| {
                    cx.stop_propagation();
                    let dragging = e.color_picker.as_ref().and_then(|p| p.dragging);
                    if let Some(part) = dragging.filter(|_| event.pressed_button == Some(MouseButton::Left)) {
                        e.pick(part, event.position, cx);
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|e, _: &MouseUpEvent, _, _| {
                        if let Some(picker) = e.color_picker.as_mut() {
                            picker.dragging = None;
                        }
                    }),
                )
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(field)
                .child(hue_bar)
                .child(alpha_bar)
                .child(footer)
                .into_any_element(),
        )
    }
}

/// A `documentColor` answer as colors, each with its notation when its
/// text is CSS the picker can write back.
fn read_document_colors(answer: &Value, rope: &Rope, server: &Arc<Server>) -> Vec<DocColor> {
    let Some(items) = answer.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let range = protocol::range_to_offsets(rope, protocol::LspRange::from_json(&item["range"])?, server.encoding);
            let part = |k: &str| item["color"][k].as_f64().map(|v| v as f32);
            let color = Color::rgba(part("red")?, part("green")?, part("blue")?, part("alpha").unwrap_or(1.));
            let text = rope.byte_slice(range.clone()).to_string();
            let format = color::parse(&text).map(|(_, f)| f);
            Some(DocColor { range, color, format })
        })
        .collect()
}

/// The colors written in the text, line by line.
pub(crate) fn scan_colors(rope: &Rope) -> Vec<DocColor> {
    let mut out = Vec::new();
    for (row, line) in rope.lines().enumerate() {
        if line.len_bytes() > MAX_SCANNED_LINE {
            continue;
        }
        let text = line.to_string();
        if !text.contains(['#', '(']) {
            continue;
        }
        let start = rope.line_to_byte(row);
        out.extend(color::find(&text).into_iter().map(|(r, color, format)| DocColor { range: start + r.start..start + r.end, color, format: Some(format) }));
    }
    out
}

/// The runs of a line with its swatches in their colors.
pub(super) fn swatch_runs(runs: Vec<TextRun>, dl: &super::DisplayLine, color_of: impl Fn(usize) -> Option<Hsla>) -> Vec<TextRun> {
    let swatches: Vec<(Range<usize>, Hsla)> = dl
        .inlays
        .iter()
        .filter(|s| dl.text.get(dl.local(s.label.clone())) == Some(SWATCH))
        .filter_map(|s| Some((dl.local(s.label.clone()), color_of(s.column)?)))
        .collect();
    if swatches.is_empty() {
        return runs;
    }
    let ranges: Vec<Range<usize>> = swatches.iter().map(|(r, _)| r.clone()).collect();
    let mut ix = 0;
    let mut pieces = super::restyle_runs(runs, &ranges, |_| {});
    // Color each swatch's piece (restyle_runs cut them out, in order).
    let mut pos = 0;
    for piece in &mut pieces {
        let range = pos..pos + piece.len;
        pos = range.end;
        while ix < swatches.len() && swatches[ix].0.end <= range.start {
            ix += 1;
        }
        if let Some((r, color)) = swatches.get(ix)
            && r.start <= range.start
            && range.end <= r.end
        {
            piece.color = *color;
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, Entity, TestAppContext};

    use super::*;
    use crate::document::Document;

    fn css(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
        let editor = cx.new(|cx| Editor::new(Document::from_text(text.to_string(), Some("a.css".into())), cx));
        cx.run_until_parked();
        editor
    }

    #[gpui::test]
    fn css_colors_get_swatches_that_follow_edits(cx: &mut TestAppContext) {
        let editor = css(cx, "a {\n  color: #f00;\n  background: rgb(0, 0, 255);\n}\n");
        let (colors, inlays) = editor.read_with(cx, |e, cx| {
            let doc = e.doc.read(cx);
            (doc.colors().iter().map(|c| c.range.clone()).collect::<Vec<_>>(), e.inlays(cx).hints.len())
        });
        assert_eq!(colors, [13..17, 33..47]);
        assert_eq!(inlays, 2);
        // Typing before one moves it and its swatch at once; the next scan agrees.
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(4..4, "  ".into())], EditKind::Other, cx));
        let moved = editor.read_with(cx, |e, cx| e.doc.read(cx).swatches().iter().map(|h| h.offset).collect::<Vec<_>>());
        assert_eq!(moved, [15, 35]);
        cx.executor().advance_clock(COLOR_PAUSE * 2);
        cx.run_until_parked();
        let scanned = editor.read_with(cx, |e, cx| e.doc.read(cx).colors().iter().map(|c| c.range.clone()).collect::<Vec<_>>());
        assert_eq!(scanned, [15..19, 35..49]);
        // Off by the setting: no swatches drawn.
        editor.update(cx, |e, _| e.file_settings.document_colors = false);
        assert_eq!(editor.read_with(cx, |e, cx| e.inlays(cx).hints.len()), 0);
    }

    #[gpui::test]
    fn picking_rewrites_the_color_in_its_notation_as_one_undo_step(cx: &mut TestAppContext) {
        let editor = css(cx, "a { color: #f00; }\n");
        let text = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.text(cx));
        editor.update(cx, |e, cx| {
            let start = e.doc.read(cx).colors()[0].range.start;
            e.layout = Some(Default::default());
            assert!(e.open_color_picker(start, Bounds::default(), cx));
            // The field and bars as drawn, 100 points wide.
            let part = |y: f32| Bounds::new(point(px(0.), px(y)), gpui::size(px(100.), px(10.)));
            e.color_picker.as_ref().unwrap().parts.set([part(0.), part(20.), part(40.)]);
            // Hue to a third of the way round: green, still short hex.
            e.pick(Part::Hue, point(px(100. / 3.), px(25.)), cx);
        });
        assert_eq!(text(cx), "a { color: #0f0; }\n");
        editor.update(cx, |e, cx| e.pick(Part::Alpha, point(px(50.), px(45.)), cx));
        assert_eq!(text(cx), "a { color: #00ff0080; }\n");
        // Closed: all of it undoes at once.
        editor.update(cx, |e, cx| {
            assert!(e.close_color_picker(cx));
            e.undo_step(cx);
        });
        assert_eq!(text(cx), "a { color: #f00; }\n");
    }

    #[test]
    fn scans_lines_for_colors_with_their_offsets() {
        let text = "a { color: #fff; }\n#id {\n  background: rgba(0, 0, 0, 0.5);\n}\n";
        let rope = Rope::from_str(text);
        let found: Vec<&str> = scan_colors(&rope).iter().map(|c| &text[c.range.clone()]).collect();
        assert_eq!(found, ["#fff", "rgba(0, 0, 0, 0.5)"]);
    }
}
