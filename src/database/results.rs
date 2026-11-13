//! The Database panel in the bottom area: what the last run did. Each query
//! is a tab with its rows in a grid (a row number column, then the columns,
//! scrolling both ways), and Output lists every statement with its outcome,
//! time and what the server said (DBMS_OUTPUT, notices, warnings). Commit
//! and Rollback act on the connection the run used, while it has changes
//! waiting for one (Oracle's). The grid is a `GridTable` (⌘C / Ctrl+C copies its selected cell).
//!
//! A run replaces the last run's Result tabs, except pinned ones, which stay
//! (with their own grid, selection and scroll) beside the new ones, numbered
//! after them. A Result tab can be renamed (double click or its right-click menu).

use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollHandle, SharedString, Window, div,
    prelude::*, px,
};

use super::backend::{Executed, Outcome};
use crate::{
    assets::Icon,
    grid::GridTable,
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
    /// A Result tab's right-click menu (its id).
    TabMenu { id: u64, position: Point<Pixels> },
    /// Rename this Result tab.
    Rename(u64),
}

/// A query's rows, as a tab.
struct ResultTab {
    id: u64,
    /// Shown as "Result {number}" unless named.
    number: usize,
    name: Option<String>,
    /// The statement, as one line (the tab's tooltip).
    sql: String,
    /// Kept when the next run starts.
    pinned: bool,
    table: Entity<GridTable>,
}

impl ResultTab {
    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("Result {}", self.number))
    }
}

/// The tab shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Result(u64),
    Output,
}

pub struct QueryResults {
    focus: FocusHandle,
    /// The connection the run used: (id, name).
    pub connection: Option<(String, String)>,
    entries: Vec<Entry>,
    /// The Result tabs: pinned ones from earlier runs, then this run's.
    results: Vec<ResultTab>,
    next_id: u64,
    tab: Tab,
    output_scroll: ScrollHandle,
    /// DML ran since the last commit or rollback.
    pub uncommitted: bool,
}

impl EventEmitter<QueryResultsEvent> for QueryResults {}

impl QueryResults {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            connection: None,
            entries: Vec::new(),
            results: Vec::new(),
            next_id: 0,
            tab: Tab::Output,
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
        self.results.retain(|r| r.pinned);
        self.tab = Tab::Output;
        cx.notify();
    }

    /// Statement `ix` finished.
    pub fn finish(&mut self, ix: usize, executed: Executed, cx: &mut Context<Self>) {
        if matches!(executed.outcome, Outcome::Changed { pending: true, .. }) {
            self.uncommitted = true;
        }
        let Some(entry) = self.entries.get_mut(ix) else { return };
        if let Outcome::Rows(grid) = &executed.outcome {
            let grid = grid.clone();
            let table = cx.new(|cx| GridTable::new(grid.columns, grid.types, grid.numeric, grid.rows, cx));
            let id = self.next_id;
            self.next_id += 1;
            let number = self.results.iter().map(|r| r.number).max().unwrap_or(0) + 1;
            self.results.push(ResultTab { id, number, name: None, sql: entry.sql.clone(), pinned: false, table });
            // The first query's rows show as they come; Output when nothing else does.
            if self.tab == Tab::Output {
                self.tab = Tab::Result(id);
            }
        }
        entry.done = Some(executed);
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

    fn result(&self, id: u64) -> Option<&ResultTab> {
        self.results.iter().find(|r| r.id == id)
    }

    /// The shown Result tab's rows.
    fn table(&self) -> Option<&Entity<GridTable>> {
        match self.tab {
            Tab::Result(id) => self.result(id).map(|r| &r.table),
            Tab::Output => None,
        }
    }

    pub fn is_pinned(&self, id: u64) -> bool {
        self.result(id).is_some_and(|r| r.pinned)
    }

    pub fn toggle_pin(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(r) = self.results.iter_mut().find(|r| r.id == id) {
            r.pinned = !r.pinned;
            cx.notify();
        }
    }

    /// The tab's label, and whether it was given a name.
    pub fn name(&self, id: u64) -> Option<(String, bool)> {
        self.result(id).map(|r| (r.label(), r.name.is_some()))
    }

    /// Names a Result tab; None (or blank) goes back to "Result N".
    pub fn rename(&mut self, id: u64, name: Option<String>, cx: &mut Context<Self>) {
        if let Some(r) = self.results.iter_mut().find(|r| r.id == id) {
            r.name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            cx.notify();
        }
    }

    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    /// Closes Result tabs: this one, or all but it (`others`), or all (`id` None).
    pub fn close_results(&mut self, id: Option<u64>, others: bool, cx: &mut Context<Self>) {
        let at = self.results.iter().position(|r| Some(r.id) == id);
        self.results.retain(|r| match id {
            None => false,
            Some(id) => (r.id == id) == others,
        });
        if let Tab::Result(shown) = self.tab
            && self.result(shown).is_none()
        {
            // The neighbor of the closed tab, else Output.
            self.tab = at.and_then(|at| self.results.get(at.min(self.results.len().saturating_sub(1)))).map_or(Tab::Output, |r| Tab::Result(r.id));
        }
        cx.notify();
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::get(cx);
        let mut tabs: Vec<(Tab, String, bool, Option<String>)> =
            self.results.iter().map(|r| (Tab::Result(r.id), r.label(), r.pinned, Some(r.sql.clone()))).collect();
        tabs.push((Tab::Output, "Output".to_string(), false, None));
        div().flex().items_center().gap_0p5().min_w_0().overflow_hidden().children(tabs.into_iter().map(|(tab, label, pinned, sql)| {
            let on = tab == self.tab;
            let failed = tab == Tab::Output && self.failed();
            let key = match tab {
                Tab::Result(id) => format!("db-tab-{id}"),
                Tab::Output => "db-tab-output".to_string(),
            };
            div()
                .id(SharedString::from(key))
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
                .when(pinned, |el| el.child(icon_el(Icon::Pin, px(11.), if on { theme.text } else { theme.text_muted }.into())))
                .child(label)
                .when_some(sql, |el, sql| el.tooltip(move |_, cx| tooltip_view(sql.clone(), cx)))
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    this.tab = tab;
                    if let Tab::Result(id) = tab
                        && event.click_count() >= 2
                    {
                        cx.emit(QueryResultsEvent::Rename(id));
                    }
                    cx.notify();
                }))
                .when_some(match tab {
                    Tab::Result(id) => Some(id),
                    Tab::Output => None,
                }, |el, id| {
                    el.on_mouse_down(MouseButton::Middle, cx.listener(move |this, _, _, cx| this.close_results(Some(id), false, cx)))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(QueryResultsEvent::TabMenu { id, position: event.position });
                            }),
                        )
                })
        }))
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
                        Outcome::Changed { rows: n, pending } => (
                            Icon::CircleCheck,
                            theme.success.into(),
                            format!("{n} row{} changed in {time}{}", if *n == 1 { "" } else { "s" }, if *pending { " (not committed)" } else { "" }),
                        ),
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
        let running = self.entries.iter().any(|e| e.done.is_none());
        let has_grid = self.table().is_some();
        let shown_result = match self.tab {
            Tab::Result(id) => Some((id, self.is_pinned(id))),
            Tab::Output => None,
        };
        let header = crate::ui::panel_header()
            .child(div().flex_shrink_0().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("DATABASE"))
            .children(self.connection.as_ref().map(|(_, name)| {
                div().flex_shrink_0().flex().items_center().gap_1().text_size(px(12.)).text_color(theme.text_dim).child(icon_el(Icon::Database, px(12.), theme.text_dim.into())).child(name.clone())
            }))
            .child(self.render_tabs(cx))
            .child(div().flex_1())
            .when(running, |el| el.child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child("Running…")))
            .when_some(shown_result, |el, (id, pinned)| {
                el.child(
                    crate::ui::icon_button("db-pin", Icon::Pin, if pinned { theme.accent } else { theme.text_dim }.into(), cx)
                        .tooltip(move |_, cx| tooltip_view(if pinned { "Unpin Result" } else { "Pin Result (the next run keeps it)" }, cx))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_pin(id, cx))),
                )
            })
            .when(has_grid, |el| {
                el.child(
                    crate::ui::icon_button("db-copy-all", Icon::Copy, theme.text_dim.into(), cx)
                        .tooltip(|_, cx| tooltip_view("Copy All Rows (tab-separated)", cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(table) = this.table() {
                                GridTable::copy_all(table, cx);
                            }
                        })),
                )
            })
            .when(self.uncommitted, |el| {
                el.child(
                    crate::ui::text_button("db-commit", "Commit", true, cx).h(px(24.)).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Commit))),
                )
                .child(crate::ui::text_button("db-rollback", "Rollback", false, cx).h(px(24.)).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Rollback))))
            })
            .child(crate::ui::panel_close_button("close-db-results", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(QueryResultsEvent::Close))));
        let body = if self.entries.is_empty() && self.results.is_empty() {
            let keys = crate::ui::binding_label_in(&crate::editor::Submit, "Editor", window).map_or(String::new(), |k| format!(" with {k}"));
            div()
                .p_3()
                .text_color(theme.text_dim)
                .child(format!("Run SQL from a .sql file{keys} (the statement at the caret, or the selection) to see its results here."))
                .into_any_element()
        } else {
            match self.table() {
                Some(table) => div().flex_1().min_h_0().child(table.clone()).into_any_element(),
                None => self.render_output(cx),
            }
        };
        div()
            .key_context("QueryResults")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::backend::Grid;

    fn rows() -> Executed {
        let grid = Grid { columns: vec!["A".into()], types: vec!["INT".into()], numeric: vec![true], rows: vec![vec![Some("1".into())]], truncated: false };
        Executed { outcome: Outcome::Rows(grid), output: Vec::new(), elapsed: Duration::ZERO }
    }

    fn labels(r: &QueryResults) -> Vec<String> {
        r.results.iter().map(ResultTab::label).collect()
    }

    #[gpui::test]
    fn pinned_results_stay_and_new_ones_number_after_them(cx: &mut gpui::TestAppContext) {
        let results = cx.new(QueryResults::new);
        results.update(cx, |r, cx| {
            let conn = || ("c".to_string(), "conn".to_string());
            r.start(conn(), vec!["select 1".into()], cx);
            r.finish(0, rows(), cx);
            assert_eq!(labels(r), ["Result 1"]);
            let first = r.results[0].id;
            assert!(r.tab == Tab::Result(first));

            // Unpinned: the next run replaces it.
            r.start(conn(), vec!["select 2".into()], cx);
            r.finish(0, rows(), cx);
            assert_eq!(labels(r), ["Result 1"]);

            // Pinned and named: it stays, the new one is Result 2 and shows.
            let first = r.results[0].id;
            r.toggle_pin(first, cx);
            r.rename(first, Some("  Products ".into()), cx);
            r.start(conn(), vec!["select 3".into()], cx);
            r.finish(0, rows(), cx);
            assert_eq!(labels(r), ["Products", "Result 2"]);
            assert!(r.tab == Tab::Result(r.results[1].id));

            // A blank name goes back to the number; closing picks a neighbor.
            r.rename(first, Some(" ".into()), cx);
            assert_eq!(labels(r), ["Result 1", "Result 2"]);
            let second = r.results[1].id;
            r.close_results(Some(second), false, cx);
            assert!(r.tab == Tab::Result(first));
            r.close_results(None, false, cx);
            assert!(r.tab == Tab::Output && r.results.is_empty());
        });
    }
}
