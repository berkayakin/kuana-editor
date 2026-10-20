//! The Problems and Find panels' place in the bottom area (see
//! `crate::problems_panel`, `crate::results_panel`).

use super::*;
use crate::results_panel::{ResultNode, ResultTarget};

impl Workspace {
    /// The panel on an island, as tall as the bottom area, with its grab edge.
    pub(super) fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        area(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(cached(&self.problems))
            .child(self.resize_handle(Edge::Problems, cx))
    }

    /// The Find panel, the same way.
    pub(super) fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        area(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(cached(&self.results))
            .child(self.resize_handle(Edge::Problems, cx))
    }

    /// Shows results in the Find panel and gives it the keyboard.
    pub(super) fn show_results(&mut self, title: SharedString, nodes: Vec<ResultNode>, window: &mut Window, cx: &mut Context<Self>) {
        self.results.update(cx, |p, cx| p.show(title, nodes, Vec::new(), true, cx));
        self.has_results = true;
        self.bottom = Some(BottomPanel::Results);
        window.focus(&self.results.focus_handle(cx));
        cx.notify();
    }

    pub(super) fn hide_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_bottom(BottomPanel::Results);
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(super) fn toggle_results(&mut self, _: &ToggleResults, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows(BottomPanel::Results) {
            self.hide_results(window, cx);
        } else {
            self.bottom = Some(BottomPanel::Results);
            window.focus(&self.results.focus_handle(cx));
            cx.notify();
        }
    }

    /// Opens a result's place; `focus`: in its editor, else the panel keeps
    /// the keyboard (moving through results with the arrows).
    pub(super) fn open_result(&mut self, target: ResultTarget, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(target.path.clone(), window, cx);
        if let Some(editor) = self.tab_for(&target.path, cx) {
            editor.update(cx, |e, cx| {
                let offset = protocol::position_to_offset(e.doc.read(cx).rope(), target.position, target.encoding);
                e.go_to(offset, window, cx);
            });
        }
        if !focus {
            window.focus(&self.results.focus_handle(cx));
        }
    }

    pub(super) fn next_result(&mut self, _: &NextResult, window: &mut Window, cx: &mut Context<Self>) {
        self.step_result(true, window, cx);
    }

    pub(super) fn previous_result(&mut self, _: &PreviousResult, window: &mut Window, cx: &mut Context<Self>) {
        self.step_result(false, window, cx);
    }

    /// Next / Previous Result: the Find panel's next place, in the editor.
    fn step_result(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.results.update(cx, |p, cx| p.step_result(forward, cx)) else { return };
        if !self.shows(BottomPanel::Results) {
            self.bottom = Some(BottomPanel::Results);
            cx.notify();
        }
        self.open_result(target, true, window, cx);
    }
}
