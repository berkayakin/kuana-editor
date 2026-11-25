//! The Agents view (a sidebar view, on either side): New Session, and the
//! project's sessions, newest first, each with a light for what it's doing
//! (working, waiting for you, failed), its agent and how long ago it was
//! active. Choosing an agent, opening and deleting go to the workspace as
//! [`AgentsViewEvent`]s.

use std::path::PathBuf;

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};

use super::session::{Session, State};
use super::store::{self, AgentStore};
use crate::{
    assets::Icon,
    theme::Theme,
    ui::{HoverFill as _, icon_button, icon_el, tooltip_view},
};

pub enum AgentsViewEvent {
    /// New Session: the agent menu, under the button.
    NewSession(gpui::Point<Pixels>),
    /// Start a session with this agent now.
    Start(String),
    Open(String),
    Delete(String),
}

pub struct AgentsView {
    folders: Vec<PathBuf>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    watched: Vec<Subscription>,
    watched_ids: Vec<gpui::EntityId>,
    _store: Option<Subscription>,
}

impl EventEmitter<AgentsViewEvent> for AgentsView {}

impl AgentsView {
    pub fn new(cx: &mut Context<Self>) -> AgentsView {
        let store_sub = store::store(cx).map(|s| cx.observe(&s, |_, _, cx| cx.notify()));
        AgentsView { folders: Vec::new(), scroll: ScrollHandle::new(), focus: cx.focus_handle(), watched: Vec::new(), watched_ids: Vec::new(), _store: store_sub }
    }

    pub fn set_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.folders != folders {
            self.folders = folders;
            cx.notify();
        }
    }

    /// Redraws when a live session's state changes.
    fn watch(&mut self, store: &Entity<AgentStore>, cx: &mut Context<Self>) {
        let live: Vec<Entity<Session>> = store.read(cx).live().to_vec();
        let ids: Vec<gpui::EntityId> = live.iter().map(|s| s.entity_id()).collect();
        if ids == self.watched_ids {
            return;
        }
        self.watched = live.iter().map(|s| cx.observe(s, |_, _, cx| cx.notify())).collect();
        self.watched_ids = ids;
    }
}

impl Focusable for AgentsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AgentsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let Some(store) = store::store(cx) else { return div().into_any_element() };
        self.watch(&store, cx);
        let rows = store.read(cx).sessions_in(&self.folders, cx);
        // The quick-start buttons, only for an empty list.
        let installed: Vec<(String, String)> =
            if rows.is_empty() { store.read(cx).choices().into_iter().filter(|c| c.installed).map(|c| (c.id, c.name)).collect() } else { Vec::new() };
        let now = super::history::now();
        let header = crate::ui::view_header()
            .child(div().pl(px(6.)).flex_1().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("AGENTS"))
            .child(
                icon_button("agents-new", Icon::Plus, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("New Session", cx))
                    .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(AgentsViewEvent::NewSession(e.position())))),
            );
        let list = div()
            .id("agents-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_1()
            .pb_2()
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .px_2()
                        .pt_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().text_color(theme.text_dim).whitespace_normal().child("Start a session with an agent: it reads, edits and runs things in this project while you watch."))
                        .children(installed.iter().map(|(id, name)| {
                            let id = id.clone();
                            div()
                                .id(SharedString::from(format!("agents-start-{id}")))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .h(px(28.))
                                .rounded(px(8.))
                                .cursor_pointer()
                                .border_1()
                                .border_color(theme.border_subtle)
                                .child(icon_el(Icon::Sparkle, px(14.), theme.accent.into()))
                                .child(div().text_color(theme.text).child(name.clone()))
                                .row_hover(theme.hover)
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(AgentsViewEvent::Start(id.clone()))))
                        })),
                )
            })
            .children(rows.into_iter().map(|row| {
                let state = row.live.as_ref().map(|s| s.read(cx).state.clone());
                let (light, tip): (Hsla, &str) = match &state {
                    Some(State::Running) | Some(State::Connecting) => (theme.accent.into(), "Working"),
                    Some(State::Waiting) | Some(State::NeedsAuth) => (theme.warning.into(), "Waiting for you"),
                    Some(State::Failed(_)) => (theme.error.into(), "Failed"),
                    Some(State::Idle) => (theme.success.into(), "Open"),
                    None => (Hsla::from(theme.text_dim).alpha(0.5), "Saved"),
                };
                let id = row.summary.id.clone();
                let delete = id.clone();
                div()
                    .id(SharedString::from(format!("agents-row-{id}")))
                    .group("agents-row")
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .row_hover(theme.hover)
                    .child(div().id(SharedString::from(format!("agents-light-{id}"))).size(px(7.)).flex_shrink_0().rounded_full().bg(light).tooltip(move |_, cx| tooltip_view(tip, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_color(theme.text).child(row.summary.title.clone()))
                            .child(div().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(format!("{} · {}", row.summary.agent_name, ago(now, row.summary.updated)))),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover("agents-row", |s| s.visible())
                            .child(
                                icon_button(SharedString::from(format!("agents-delete-{id}")), Icon::Trash, theme.text_dim.into(), cx)
                                    .tooltip(|_, cx| tooltip_view("Delete Session", cx))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(AgentsViewEvent::Delete(delete.clone()));
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(AgentsViewEvent::Open(id.clone()))))
            }));
        div()
            .size_full()
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().relative().flex().flex_col().child(list).child(crate::ui::scrollbar("agents-scrollbar", &self.scroll, cx)))
            .into_any_element()
    }
}

/// "now", "5m", "3h", "2d" since a time.
fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ages_read_short() {
        assert_eq!(super::ago(100, 90), "now");
        assert_eq!(super::ago(1000, 400), "10m ago");
        assert_eq!(super::ago(100_000, 0), "1d ago");
    }
}
