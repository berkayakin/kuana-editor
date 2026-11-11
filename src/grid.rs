//! A table of text cells as a view: a row number column, then the columns,
//! scrolling both ways, with a header that stays on top. Click a cell to
//! select it; ⌘C / Ctrl+C copies it, `copy_all` the whole table as
//! tab-separated text. Used by the Database panel's results and by
//! extensions' panels.

use std::ops::Range;

use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, FontWeight, Hsla, Render, ScrollHandle, UniformListScrollHandle, Window, div, prelude::*,
    px, uniform_list,
};

use crate::{
    editor::Copy,
    theme::Theme,
    ui::{HoverFill, tooltip_view},
};

const ROW: f32 = 24.;
const NUMBER_WIDTH: f32 = 48.;

pub struct GridTable {
    focus: FocusHandle,
    columns: Vec<String>,
    /// Shown in a column's tooltip (a database type), if known.
    types: Vec<String>,
    /// Columns shown right-aligned (numbers).
    numeric: Vec<bool>,
    /// Cells as text; None is NULL / empty.
    rows: Vec<Vec<Option<String>>>,
    widths: Vec<f32>,
    /// The selected cell: (row, column).
    cell: Option<(usize, usize)>,
    scroll: UniformListScrollHandle,
    hscroll: ScrollHandle,
}

impl GridTable {
    pub fn new(columns: Vec<String>, types: Vec<String>, numeric: Vec<bool>, rows: Vec<Vec<Option<String>>>, cx: &mut Context<Self>) -> Self {
        let widths = column_widths(&columns, &rows);
        GridTable {
            focus: cx.focus_handle(),
            columns,
            types,
            numeric,
            rows,
            widths,
            cell: None,
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
        }
    }

    /// The whole table as tab-separated text, with its header.
    pub fn text(&self) -> String {
        let mut text = self.columns.join("\t");
        for row in &self.rows {
            text.push('\n');
            text.push_str(&row.iter().map(|v| v.as_deref().unwrap_or("").replace(['\t', '\n'], " ")).collect::<Vec<_>>().join("\t"));
        }
        text
    }

    /// Copies the whole table (see `text`).
    pub fn copy_all(this: &gpui::Entity<Self>, cx: &mut App) {
        let text = this.read(cx).text();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some((row, column)) = self.cell else { return };
        let text = self.rows.get(row).and_then(|r| r.get(column)).cloned().flatten().unwrap_or_default();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn render_row(&self, r: usize, focused: bool, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let row = &self.rows[r];
        div()
            .id(("grid-row", r))
            .flex()
            .h(px(ROW))
            .w_full()
            .row_hover(theme.hover)
            .child(
                div()
                    .w(px(NUMBER_WIDTH))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_end()
                    .pr_2()
                    .text_size(px(11.5))
                    .text_color(theme.text_dim)
                    .child((r + 1).to_string()),
            )
            .children(row.iter().zip(&self.widths).enumerate().map(|(c, (value, width))| {
                let selected = self.cell == Some((r, c));
                let numeric = self.numeric.get(c).copied().unwrap_or(false);
                div()
                    .id(("grid-cell", r * 10_000 + c))
                    .w(px(*width))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .when(numeric, |el| el.justify_end())
                    .px_2()
                    .border_l_1()
                    .border_color(Hsla::from(theme.border_subtle).alpha(0.6))
                    .font_family(theme.mono_font.clone())
                    .text_size(px(12.))
                    .when(selected, |el| {
                        el.bg(if focused { theme.accent_soft } else { theme.active }).border_1().border_color(Hsla::from(theme.accent).alpha(0.6))
                    })
                    .map(|el| match value {
                        Some(text) => el.text_color(theme.text).child(div().truncate().child(first_line(text))),
                        None => el.text_color(theme.text_dim).child("(null)"),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus);
                        this.cell = Some((r, c));
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }
}

/// Widths that fit a table's header and first rows, within limits.
fn column_widths(columns: &[String], rows: &[Vec<Option<String>>]) -> Vec<f32> {
    const CHAR: f32 = 7.4;
    (0..columns.len())
        .map(|c| {
            let longest = rows
                .iter()
                .take(200)
                .map(|r| r.get(c).and_then(|v| v.as_deref()).map_or(6, |v| first_line(v).chars().count()))
                .chain([columns[c].chars().count() + 1])
                .max()
                .unwrap_or(4);
            (longest as f32 * CHAR + 18.).clamp(56., 380.)
        })
        .collect()
}

/// A cell's first line (a cell shows one), marked when there's more.
fn first_line(text: &str) -> String {
    match text.find(['\n', '\r']) {
        Some(i) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

impl Focusable for GridTable {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for GridTable {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        if self.columns.is_empty() {
            return div().p_3().text_color(theme.text_dim).child("No columns").into_any_element();
        }
        let focused = self.focus.contains_focused(window, cx);
        let total: f32 = NUMBER_WIDTH + self.widths.iter().sum::<f32>();
        let header = div()
            .flex()
            .flex_shrink_0()
            .h(px(ROW + 2.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .bg(theme.panel)
            .child(div().w(px(NUMBER_WIDTH)).flex_shrink_0())
            .children(self.columns.iter().zip(&self.widths).enumerate().map(|(i, (name, width))| {
                let tip = match self.types.get(i) {
                    Some(ty) if !ty.is_empty() => format!("{name}  {ty}"),
                    _ => name.clone(),
                };
                div()
                    .id(("grid-col", i))
                    .w(px(*width))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_2()
                    .border_l_1()
                    .border_color(theme.border_subtle)
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                    .child(div().truncate().child(name.clone()))
            }));
        let list = uniform_list(
            "grid-rows",
            self.rows.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let theme = Theme::get(cx).clone();
                range.filter(|&r| r < this.rows.len()).map(|r| this.render_row(r, focused, &theme, cx)).collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .min_h_0()
        .w(px(total));
        div()
            .key_context("Grid")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::copy))
            .relative()
            .size_full()
            .child(
                div()
                    .id("grid-h")
                    .size_full()
                    .overflow_x_scroll()
                    .track_scroll(&self.hscroll)
                    .child(div().flex().flex_col().h_full().w(px(total)).child(header).child(list)),
            )
            .child(crate::ui::list_scrollbar("grid-scrollbar", &self.scroll, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_fit_within_limits() {
        let columns = vec!["ID".to_string(), "DESCRIPTION".to_string()];
        let rows = vec![vec![Some("1".into()), Some("x".repeat(200))], vec![None, Some("short\nsecond line".into())]];
        let widths = column_widths(&columns, &rows);
        // The widest cell is the NULL, shown as "(null)": 6 characters.
        assert!((widths[0] - (6. * 7.4 + 18.)).abs() < 0.01, "{}", widths[0]);
        assert_eq!(widths[1], 380.);
        assert_eq!(first_line("a\nb"), "a…");
    }
}
