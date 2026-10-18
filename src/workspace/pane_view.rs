//! The editor area: panes side by side or stacked with resize handles
//! between them, the welcome screen in a lone empty pane, and the Git diff
//! over them all. Each pane draws itself (`pane.rs`).

use super::*;

impl Workspace {
    /// The editor area: panes side by side or stacked, with dividers.
    pub(super) fn render_panes(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.panes_bounds.clone();
        // The start screen, or ways to open a file, while there's nothing open.
        let lone_empty = self.panes.len() == 1 && self.active_pane.read(cx).is_empty();
        div()
            .relative()
            .flex()
            .gap(px(GAP))
            .size_full()
            .when(self.pane_axis == PaneAxis::Column, |el| el.flex_col())
            .child(
                gpui::canvas(move |b, window, _| track_bounds(&bounds, b, window), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children(self.panes.iter().enumerate().map(|(ix, pane)| {
                let share = relative(self.pane_sizes.get(ix).copied().unwrap_or(1.));
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .min_h_0()
                    .map(|el| match self.pane_axis {
                        PaneAxis::Row => el.h_full().w(share),
                        PaneAxis::Column => el.w_full().h(share),
                    })
                    .child(if lone_empty { self.render_lone_pane(window, cx) } else { cached(pane).into_any_element() })
                    .when(ix + 1 < self.panes.len(), |el| el.child(self.resize_handle(Edge::Pane(ix), cx)))
            }))
            .when_some(self.diff_view(), |el, view| el.child(self.render_diff_overlay(view, cx)))
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
            .child(window_drag_area("tabs-room").flex_shrink_0().h(px(TAB_STRIP_HEIGHT - 1.)))
            .child(island(&theme).flex_1().min_w_0().min_h_0().child(div().flex_1().min_h_0().overflow_hidden().child(content)))
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

    pub(super) fn diff_view(&self) -> Option<Entity<DiffView>> {
        match &self.modals.current {
            Some(Modal::Diff(view)) => Some(view.clone()),
            _ => None,
        }
    }

    /// The Git diff covers only the editor area, so the sidebar (Commit view)
    /// and the bottom panels (Git log) stay usable beside it.
    /// The Git diff over the editor area, with a tab of its own in place of
    /// the file tabs (hidden meanwhile, so they don't seem to control it).
    pub(super) fn render_diff_overlay(&self, view: Entity<DiffView>, cx: &App) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (path, rel) = view.read(cx).tab_file();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone());
        let edge = crate::ui::island_edge(&theme);
        let close_view = view.clone();
        let tab = div()
            .id("diff-tab")
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            .h_full()
            .pl_3()
            .pr_1p5()
            .max_w(px(360.))
            .text_color(theme.text)
            .font_weight(FontWeight::MEDIUM);
        let tab = tab_shape(tab, true, false, false, "diff-tab".into(), edge, &theme)
            .tooltip(move |_, cx| tooltip_view(format!("Changes in {rel}"), cx))
            .child(icon_el(Icon::GitCompare, px(14.), theme.accent.into()))
            .child(div().min_w_0().truncate().child(name))
            .child(
                div()
                    .id("close-diff-tab")
                    .flex_shrink_0()
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover_bg(theme.active)
                    .on_click(move |_, _, cx| close_view.update(cx, |v, cx| v.close(cx)))
                    .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
            );
        gpui::deferred(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .child(div().flex_shrink_0().h(px(TAB_STRIP_HEIGHT - 1.)))
                .child(
                    island(&theme)
                        .id("diff-layer")
                        .flex_1()
                        .min_h_0()
                        .occlude()
                        .child(div().size_full().overflow_hidden().child(view)),
                )
                // After the island, so the tab paints over its top border.
                .child(
                    div()
                        .id("diff-tab-strip")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(TAB_STRIP_HEIGHT))
                        .flex()
                        .items_end()
                        .pt(px(TAB_STRIP_TOP))
                        .pl(px(TAB_STRIP_INSET))
                        .occlude()
                        .child(tab)
                        .child(super::title_bar::window_drag_area("diff-tabs-free").flex_1().min_w_0()),
                ),
        )
        .with_priority(1)
    }
}
