//! The Database panel in the bottom area: what the last run did. Each query
//! is a tab with its rows in a grid (a row number column, then the columns,
//! scrolling both ways), and Output lists every statement with its outcome,
//! time and DBMS_OUTPUT lines. Commit and Rollback act on the connection the
//! run used; ⌘C / Ctrl+C copies the selected cell.

use std::{ops::Range, time::Duration};

use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Render, ScrollHandle, SharedString,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::oracle::{Executed, Grid, Outcome};
use crate::{
    assets::Icon,
    editor::Copy,
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

pub struct Entry {
    /// The statement, as one line.
    pub sql: String,
    /// None while it runs.
    pub done: Option<Executed>,
}

pub enum QueryResultsEvent {
    Close,
    Commit,
    Rollback,
    /// Show this statement in its editor (its index in the run).
    Reveal(usize),
}

pub struct QueryResults {
    focus: FocusHandle,
    /// The connection the run used: (id, name).
    pub connection: Option<(String, String)>,
    entries: Vec<Entry>,
    /// The tab shown: a query's entry index, or None for Output.
    tab: Option<usize>,
    /// The selected cell: (row, column) of the shown grid.
    cell: Option<(usize, usize)>,
    /// Column widths of the shown grid.
    widths: Vec<f32>,
    scroll: UniformListScrollHandle,
    hscroll: ScrollHandle,
    output_scroll: ScrollHandle,
    /// DML ran since the last commit or rollback.
    pub uncommitted: bool,
}

impl EventEmitter<QueryResultsEvent> for QueryResults {}

const ROW: f32 = 24.;
const NUMBER_WIDTH: f32 = 48.;

impl QueryResults {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            connection: None,
            entries: Vec::new(),
            tab: None,
            cell: None,
            widths: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            hscroll: ScrollHandle::new(),
            output_scroll: ScrollHandle::new(),
            uncommitted: false,
        }
    }

    /// A new run of these statements (shown as running).
    pub fn start(&mut self, connection: (String, String), statements: Vec<String>, cx: &mut Context<Self>) {
        if self.connection.as_ref().map(|c| &c.0) != Some(&connection.0) {
            self.uncommitted = false;
        }
        self.connection = Some(connection);
        self.entries = statements.into_iter().map(|sql| Entry { sql: one_line(&sql), done: None }).collect();
        self.tab = None;
        self.cell = None;
        self.widths.clear();
        cx.notify();
    }

    /// Statement `ix` finished.
    pub fn finish(&mut self, ix: usize, executed: Executed, cx: &mut Context<Self>) {
        if matches!(executed.outcome, Outcome::Changed(_)) {
            self.uncommitted = true;
        }
        let Some(entry) = self.entries.get_mut(ix) else { return };
        let query = matches!(executed.outcome, Outcome::Rows(_));
        entry.done = Some(executed);
        // The first query's rows show as they come; Output when nothing else does.
        if query && self.tab.is_none() {
            self.show_tab(Some(ix));
        }
        cx.notify();
    }

    pub fn committed(&mut self, cx: &mut Context<Self>) {
        self.uncommitted = false;
        cx.notify();
    }

    /// Whether the run ended with only a failure or output to read (the
    /// workspace then gives it the keyboard).
    pub fn failed(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.done.as_ref().map(|d| &d.outcome), Some(Outcome::Failed { .. })))
    }

    fn grid(&self) -> Option<&Grid> {
        match self.entries.get(self.tab?)?.done.as_ref()?.outcome {
            Outcome::Rows(ref grid) => Some(grid),
            _ => None,
        }
    }

    fn show_tab(&mut self, tab: Option<usize>) {
        self.tab = tab;
        self.cell = None;
        self.widths = self.grid().map(column_widths).unwrap_or_default();
        self.scroll = UniformListScrollHandle::new();
        self.hscroll = ScrollHandle::new();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(grid) = self.grid() else { return };
        let text = match self.cell {
            Some((row, column)) => grid.rows.get(row).and_then(|r| r.get(column)).cloned().flatten().unwrap_or_default(),
            None => return,
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// The shown grid as tab-separated text, with its header.
    fn copy_all(&self, cx: &mut App) {
        let Some(grid) = self.grid() else { return };
        let mut text = grid.columns.join("\t");
        for row in &grid.rows {
            text.push('\n');
            text.push_str(&row.iter().map(|v| v.as_deref().unwrap_or("").replace(['\t', '\n'], " ")).collect::<Vec<_>>().join("\t"));
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::get(cx);
        let mut tabs: Vec<(Option<usize>, String)> = Vec::new();
        let mut n = 0;
        for (ix, entry) in self.entries.iter().enumerate() {
            if matches!(entry.done.as_ref().map(|d| &d.outcome), Some(Outcome::Rows(_))) {
                n += 1;
                tabs.push((Some(ix), format!("Result {n}")));
            }
        }
        tabs.push((None, "Output".to_string()));
        div().flex().items_center().gap_0p5().min_w_0().overflow_hidden().children(tabs.into_iter().map(|(tab, label)| {
            let on = tab == self.tab;
            let failed = tab.is_none() && self.failed();
            div()
                .id(SharedString::from(format!("db-tab-{label}")))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .h(px(24.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on { theme.text } else { theme.text_muted })
                .hover_fill(if on { Hsla::from(theme.active) } else { Hsla::from(theme.hover).alpha(0.) }, if on { theme.active } else { theme.hover })
                .when(failed, |el| el.child(icon_el(Icon::CircleAlert, px(12.), theme.error.into())))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.show_tab(tab);
                    cx.notify();
                }))
        }))
    }

    fn render_grid(&self, grid: &Grid, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::get(cx).clone();
        let total: f32 = NUMBER_WIDTH + self.widths.iter().sum::<f32>();
        let header = div()
            .flex()
            .flex_shrink_0()
            .h(px(ROW + 2.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .bg(theme.panel)
            .child(div().w(px(NUMBER_WIDTH)).flex_shrink_0())
            .children(grid.columns.iter().zip(&self.widths).zip(&grid.types).enumerate().map(|(i, ((name, width), ty))| {
                let tip = format!("{name}  {ty}");
                div()
                    .id(("db-col", i))
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
        let rows = grid.rows.len();
        let list = uniform_list(
            "db-grid",
            rows,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let Some(grid) = this.grid() else { return Vec::new() };
                let theme = Theme::get(cx).clone();
                range.filter(|&r| r < grid.rows.len()).map(|r| this.render_grid_row(grid, r, focused, &theme, cx)).collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .min_h_0()
        .w(px(total));
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("db-grid-h")
                    .size_full()
                    .overflow_x_scroll()
                    .track_scroll(&self.hscroll)
                    .child(div().flex().flex_col().h_full().w(px(total)).child(header).child(list)),
            )
            .child(crate::ui::list_scrollbar("db-grid-scrollbar", &self.scroll, cx))
            .into_any_element()
    }

    fn render_grid_row(&self, grid: &Grid, r: usize, focused: bool, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let row = &grid.rows[r];
        div()
            .id(("db-grid-row", r))
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
                let numeric = grid.numeric.get(c).copied().unwrap_or(false);
                div()
                    .id(("db-cell", r * 10_000 + c))
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

    fn render_output(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::get(cx).clone();
        let items = self.entries.iter().enumerate().map(|(ix, entry)| {
            let (icon, color, summary): (Icon, Hsla, String) = match entry.done.as_ref() {
                None => (Icon::Refresh, theme.text_dim.into(), "Running…".into()),
                Some(done) => {
                    let time = elapsed(done.elapsed);
                    match &done.outcome {
                        Outcome::Rows(grid) => (
                            Icon::CircleCheck,
                            theme.success.into(),
                            format!("{} row{}{} in {time}", grid.rows.len(), if grid.rows.len() == 1 { "" } else { "s" }, if grid.truncated { " (first ones)" } else { "" }),
                        ),
                        Outcome::Changed(n) => (Icon::CircleCheck, theme.success.into(), format!("{n} row{} changed in {time} (not committed)", if *n == 1 { "" } else { "s" })),
                        Outcome::Done => (Icon::CircleCheck, theme.success.into(), format!("Done in {time}")),
                        Outcome::Compiled(errors) => (
                            Icon::TriangleAlert,
                            theme.warning.into(),
                            format!("Created with {} compile error{}", errors.len(), if errors.len() == 1 { "" } else { "s" }),
                        ),
                        Outcome::Failed { message, .. } => (Icon::CircleX, theme.error.into(), message.clone()),
                    }
                }
            };
            let details: Vec<(String, Hsla)> = match entry.done.as_ref() {
                Some(done) => {
                    let mut lines = Vec::new();
                    if let Outcome::Compiled(errors) = &done.outcome {
                        for e in errors {
                            lines.push((format!("{}:{}  {}", e.line, e.column, e.message), if e.warning { theme.warning.into() } else { theme.error.into() }));
                        }
                    }
                    lines.extend(done.output.iter().map(|l| (l.clone(), Hsla::from(theme.text))));
                    lines
                }
                None => Vec::new(),
            };
            div()
                .id(("db-output", ix))
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border_subtle)
                .cursor_pointer()
                .row_hover(theme.hover)
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(QueryResultsEvent::Reveal(ix))))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(div().pt(px(2.)).child(icon_el(icon, px(14.), color)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .gap_0p5()
                                .child(div().text_size(px(12.5)).text_color(color).child(summary))
                                .child(div().truncate().font_family(theme.mono_font.clone()).text_size(px(11.5)).text_color(theme.text_dim).child(entry.sql.clone())),
                        ),
                )
                .when(!details.is_empty(), |el| {
                    el.child(
                        div()
                            .ml(px(22.))
                            .p_1p5()
                            .rounded(px(6.))
                            .bg(theme.panel)
                            .font_family(theme.mono_font.clone())
                            .text_size(px(12.))
                            .children(details.into_iter().map(|(line, color)| div().text_color(color).child(if line.is_empty() { " ".to_string() } else { line }))),
                    )
                })
        });
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(div().id("db-output-list").size_full().overflow_y_scroll().track_scroll(&self.output_scroll).children(items))
            .child(crate::ui::scrollbar("db-output-scrollbar", &self.output_scroll, cx))
            .into_any_element()
    }
}

/// Widths that fit a grid's header and first rows, within limits.
fn column_widths(grid: &Grid) -> Vec<f32> {
    const CHAR: f32 = 7.4;
    (0..grid.columns.len())
        .map(|c| {
            let longest = grid
                .rows
                .iter()
                .take(200)
                .map(|r| r.get(c).and_then(|v| v.as_deref()).map_or(6, |v| first_line(v).chars().count()))
                .chain([grid.columns[c].chars().count() + 1])
                .max()
                .unwrap_or(4);
            (longest as f32 * CHAR + 18.).clamp(56., 380.)
        })
        .collect()
}

fn first_line(text: &str) -> String {
    match text.find(['\n', '\r']) {
        Some(i) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

fn one_line(sql: &str) -> String {
    let text = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 200 { format!("{}…", text.chars().take(199).collect::<String>()) } else { text }
}

fn elapsed(d: Duration) -> String {
    if d.as_millis() < 1000 { format!("{} ms", d.as_millis()) } else { format!("{:.2} s", d.as_secs_f32()) }
}

impl Focusable for QueryResults {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for QueryResults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let focused = self.focus.contains_focused(window, cx);
        let running = self.entries.iter().any(|e| e.done.is_none());
        let has_grid = self.grid().is_some();
        let header = crate::ui::panel_header(&theme)
            .child(div().flex_shrink_0().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("DATABASE"))
            .children(self.connection.as_ref().map(|(_, name)| {
                div().flex_shrink_0().flex().items_center().gap_1().text_size(px(12.)).text_color(theme.text_dim).child(icon_el(Icon::Database, px(12.), theme.text_dim.into())).child(name.clone())
            }))
            .child(self.render_tabs(cx))
            .child(div().flex_1())
            .when(running, |el| el.child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child("Running…")))
            .when(has_grid, |el| {
                el.child(
                    crate::ui::icon_button("db-copy-all", Icon::Copy, theme.text_dim.into(), cx)
                        .tooltip(|_, cx| tooltip_view("Copy All Rows (tab-separated)", cx))
                        .on_click(cx.listener(|this, _, _, cx| this.copy_all(cx))),
                )
            })
            .when(self.uncommitted, |el| {
                el.child(
                    crate::ui::text_button("db-commit", "Commit", true, cx).h(px(24.)).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Commit))),
                )
                .child(crate::ui::text_button("db-rollback", "Rollback", false, cx).h(px(24.)).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Rollback))))
            })
            .child(crate::ui::panel_close_button("close-db-results", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Close))));
        let body = if self.entries.is_empty() {
            let keys = crate::ui::binding_label_in(&crate::editor::Submit, "Editor", window).map_or(String::new(), |k| format!(" with {k}"));
            div()
                .p_3()
                .text_color(theme.text_dim)
                .child(format!("Run SQL from a .sql file{keys} (the statement at the caret, or the selection) to see its results here."))
                .into_any_element()
        } else {
            match self.grid() {
                Some(grid) if grid.columns.is_empty() => div().p_3().text_color(theme.text_dim).child("No columns").into_any_element(),
                Some(grid) => self.render_grid(grid, focused, cx),
                None => self.render_output(cx),
            }
        };
        div()
            .key_context("QueryResults")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::copy))
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
    }
}
