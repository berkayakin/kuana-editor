//! The editor area: panes as the tree of splits arranges them (`pane_tree`),
//! with resize handles between them, and the welcome screen in a lone empty
//! pane. Each pane draws itself (`pane.rs`).

use super::*;

impl Workspace {
    /// The editor area: the panes as split.
    pub(super) fn render_panes(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The start screen, or ways to open a file, while there's nothing open.
        let lone_empty = self.center.len() == 1 && self.active_pane.read(cx).is_empty();
        div()
            .relative()
            .flex()
            .size_full()
            .child(self.render_member(self.center.root(), lone_empty, window, cx))
    }

    /// A pane (cached: it redraws when it changes, not with the workspace),
    /// or a split's members side by side or stacked, each in a slot as big as
    /// its share, with a resize handle in the gap after it.
    fn render_member(&self, member: &Member<Entity<Pane>>, lone_empty: bool, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let split = match member {
            Member::Pane(_) if lone_empty => return self.render_lone_pane(window, cx),
            Member::Pane(pane) => return cached(pane).into_any_element(),
            Member::Split(split) => split,
        };
        let bounds = split.bounds.clone();
        let count = split.members.len();
        div()
            .relative()
            .flex()
            .gap(px(layout_gap(Theme::get(cx))))
            .size_full()
            .min_w_0()
            .min_h_0()
            .when(split.axis == Axis::Column, |el| el.flex_col())
            // Where it is, for drags of its handles.
            .child(gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full())
            .children(split.members.iter().zip(&split.shares).enumerate().map(|(ix, (member, share))| {
                let mut slot = div().relative().flex().flex_col().flex_basis(px(0.)).min_w_0().min_h_0();
                // Grow factors summing to the member count (not to 1, which a
                // rounding error below would leave room unfilled at).
                slot.style().flex_grow = Some(share * count as f32);
                slot.style().flex_shrink = Some(1.);
                slot.child(self.render_member(member, lone_empty, window, cx)).when(ix + 1 < count, |el| {
                    el.child(self.resize_handle(Edge::Split { split: split.id, axis: split.axis, ix }, cx))
                })
            }))
            .into_any_element()
    }

    /// The only pane, without tabs: the start screen when there's no
    /// project yet, else ways to open a file. Files dropped on it open.
    fn render_lone_pane(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let content = if self.root.is_none() {
            self.render_welcome(window, cx).into_any_element()
        } else {
            self.render_no_open_files(window, cx).into_any_element()
        };
        let drop_group = SharedString::from("lone-pane-drop");
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            // The tab strip's room on the chrome, so the island lines up with
            // the sidebar's; along the window's top, it moves the window.
            .child(tab_room(window_drag_area("tabs-room"), &theme))
            .child(area(&theme).flex_1().min_w_0().min_h_0().child(div().flex_1().min_h_0().overflow_hidden().child(content)))
            .group(drop_group.clone())
            .child(
                file_drop_overlay(drop_group, &theme)
                    .on_drop(cx.listener(|ws, paths: &gpui::ExternalPaths, window, cx| {
                        cx.stop_propagation();
                        ws.open_dropped(paths.paths(), window, cx);
                    }))
                    .on_drop(cx.listener(|ws, entry: &crate::project_tree::DraggedEntry, window, cx| {
                        cx.stop_propagation();
                        ws.open_path(entry.path.clone(), window, cx);
                    })),
            )
            .into_any_element()
    }
}
