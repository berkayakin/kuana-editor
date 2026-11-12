//! The sidebar: Files and Commit views on their island (switched from the
//! activity bar beside it, `activity_bar.rs`).

use super::*;

impl Workspace {
    /// The Commit view's header: branch (opens Branches…), fetch, pull, push.
    pub(super) fn render_git_toolbar(&self, cx: &mut Context<Self>) -> [AnyElement; 4] {
        let theme = Theme::get(cx).clone();
        let s = &self.git_status;
        let branch = self.branch_label();
        let button = |id: &'static str, icon: Icon, tip: &'static str, count: u32, op: GitOp, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .gap_0p5()
                .h(px(24.))
                .min_w(px(24.))
                .px_1()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(move |ws, _, window, cx| ws.run_git(op.clone(), window, cx)))
                .child(icon_el(icon, px(14.), theme.text_muted.into()))
                .when(count > 0, |el| el.child(count.to_string()))
                .into_any_element()
        };
        [
            div()
                .id("sidebar-branch")
                .flex()
                .items_center()
                .gap_1p5()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .px_1p5()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.5))
                .hover_bg(theme.hover)
                .tooltip(|window, cx| status_tooltip("Branches…", &GitBranches, window, cx))
                .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
                .child(icon_el(Icon::GitBranch, px(14.), theme.text_dim.into()))
                .child(div().truncate().text_color(theme.text).child(branch))
                .into_any_element(),
            button("sidebar-fetch", Icon::Refresh, "Fetch", 0, GitOp::Fetch, cx),
            button("sidebar-pull", Icon::ArrowDown, "Update Project (Pull)", s.behind, GitOp::Pull(None), cx),
            button("sidebar-push", Icon::ArrowUp, "Push", s.ahead, GitOp::Push, cx),
        ]
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // The Commit view has a toolbar (its branch).
        let header = (self.sidebar_mode == SidebarMode::Commit && self.repo.is_some()).then(|| {
            div()
                .flex()
                .items_center()
                .gap_0p5()
                .h(px(30.))
                .px_1()
                .mb_1()
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border_subtle)
                .children(self.render_git_toolbar(cx))
        });

        let body: AnyElement = if self.sidebar_mode == SidebarMode::Commit {
            div().flex_1().min_h_0().child(cached(&self.git_panel)).into_any_element()
        } else if self.sidebar_mode == SidebarMode::Database {
            div().flex_1().min_h_0().child(cached(&self.database)).into_any_element()
        } else if let SidebarMode::Extension(slot) = self.sidebar_mode
            && let Some(view) = self.render_extension_sidebar(slot)
        {
            view
        } else if self.root.is_some() {
            div().flex_1().min_h_0().child(cached(&self.tree)).into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .text_color(theme.text_muted)
                .child("No folder opened.")
                .child(self.primary_button("open-folder-side", Icon::FolderSearch, "Open Folder", cx, |ws, _, window, cx| {
                    ws.open_folder(&OpenFolder, window, cx)
                }))
                .child(self.primary_button("clone-side", Icon::Download, "Clone Repository", cx, |ws, _, window, cx| {
                    ws.git_clone(&GitClone, window, cx)
                }))
                .into_any_element()
        };

        crate::ui::area_joined(&theme, self.view_tab_joint(cx))
            .w(self.shown_sidebar_width())
            .flex_shrink_0()
            .h_full()
            .children(header)
            .child(body)
            // Deferred so the grab area sits above the editor it overlaps.
            .child(gpui::deferred(self.resize_handle(Edge::Sidebar, cx)))
    }

    /// Whether the sidebar sits right of the editor area (`sidebar_dock`).
    pub(super) fn sidebar_on_right(&self, cx: &App) -> bool {
        Settings::get(cx).sidebar_dock() == crate::settings::SidebarDock::Right
    }

    /// Toggle Sidebar Position, and Move to Right / Left on the sidebar's tabs.
    pub(super) fn toggle_sidebar_position(&mut self, _: &ToggleSidebarPosition, window: &mut Window, cx: &mut Context<Self>) {
        let dock = Settings::get(cx).sidebar_dock().other();
        if let Err(err) = Settings::set_sidebar_dock(dock) {
            let _ = window.prompt(PromptLevel::Warning, "Couldn't save the sidebar's position", Some(&err.to_string()), &["OK"], cx);
        }
        let problems = settings::reload_now(cx);
        self.report_config_problems(problems, cx);
        cx.notify();
    }

    /// Toggle Classic Layout: the window between the Modern and Classic styles.
    pub(super) fn toggle_classic_layout(&mut self, _: &ToggleClassicLayout, window: &mut Window, cx: &mut Context<Self>) {
        let style = match Settings::get(cx).ui_style() {
            settings::UiStyle::Modern => settings::UiStyle::Classic,
            settings::UiStyle::Classic => settings::UiStyle::Modern,
        };
        if let Err(err) = Settings::set_ui_style(style) {
            let _ = window.prompt(PromptLevel::Warning, "Couldn't save the layout", Some(&err.to_string()), &["OK"], cx);
        }
        let problems = settings::reload_now(cx);
        self.report_config_problems(problems, cx);
        cx.notify();
    }

    /// The activity bar's right-click menu: move the sidebar to the other side.
    pub(super) fn open_sidebar_tab_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let label = if self.sidebar_on_right(cx) { "Move to Left" } else { "Move to Right" };
        let icon = if self.sidebar_on_right(cx) { Icon::PanelLeft } else { Icon::PanelRight };
        let entries = vec![MenuEntry::item(label, icon)];
        let actions = vec![Some(MenuAction::Run(Box::new(ToggleSidebarPosition)))];
        self.show_context_menu(entries, actions, position, None, window, cx);
    }
}
