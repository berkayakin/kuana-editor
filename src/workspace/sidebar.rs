//! The sidebar: Files and Commit views on their island.

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

    pub(super) fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Under the Files / Commit switch, the Commit view has a toolbar (its branch).
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

        // Files and Commit are tabs on the chrome, like the editor's, the
        // current one joined to the island.
        div()
            .relative()
            .flex()
            .flex_col()
            .w(self.sidebar_width)
            .flex_shrink_0()
            .h_full()
            .child(tab_room(div(), &theme))
            .child(
                area(&theme)
                    .flex_1()
                    .min_h_0()
                    .children(header)
                    .child(body)
                    // Deferred so the grab area sits above the editor it overlaps.
                    .child(gpui::deferred(self.resize_handle(Edge::Sidebar, cx))),
            )
            // After the island, so the current tab paints over its top border.
            .child(self.render_sidebar_tabs(window, cx))
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

    /// The sidebar tabs' right-click menu: move the sidebar to the other side.
    fn open_sidebar_tab_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let label = if self.sidebar_on_right(cx) { "Move to Left" } else { "Move to Right" };
        let icon = if self.sidebar_on_right(cx) { Icon::PanelLeft } else { Icon::PanelRight };
        let entries = vec![MenuEntry::item(label, icon)];
        let actions = vec![Some(MenuAction::Run(Box::new(ToggleSidebarPosition)))];
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The sidebar's two views as tabs above its island, shaped like the
    /// editor's (`tab_shape`), without close buttons. In a narrow sidebar they
    /// give up their names: the other tab first, then the current one too
    /// (their tooltips still name them). `SIDEBAR_MIN_WIDTH` keeps two icons in.
    pub(super) fn render_sidebar_tabs(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut theme = Theme::get(cx).clone();
        // Lit with the tree's island while a drag targets the project folder.
        if self.tree_root_drop && self.sidebar_mode != SidebarMode::Commit {
            theme.background = crate::ui::drop_lift(&theme).1.into();
        }
        let files_on = self.sidebar_mode == SidebarMode::Files;
        let changes = self.git_status.files.len();
        let edge = crate::ui::island_edge(&theme);
        let workspace = cx.entity();
        // Padding, icon and gap around a name; the change count adds a gap and itself.
        let count = (changes > 0).then(|| changes.to_string());
        let measured = measure_tab_names(&["Files".to_string(), "Commit".to_string(), count.clone().unwrap_or_default()], window, &theme);
        let files_width = SIDEBAR_TAB_CHROME + measured[0].0;
        let commit_width = SIDEBAR_TAB_CHROME + measured[1].0 + if count.is_some() { 6. + measured[2].0 } else { 0. };
        let available = f32::from(self.sidebar_width) - 2. * tab_strip_inset(&theme);
        let (active_width, other_width) = if files_on { (files_width, commit_width) } else { (commit_width, files_width) };
        let (active_named, other_named) = if active_width + other_width <= available {
            (true, true)
        } else {
            (active_width + TAB_ICON_WIDTH <= available, false)
        };
        let (files_named, commit_named) = if files_on { (active_named, other_named) } else { (other_named, active_named) };
        let tab = |id: &'static str, icon: Icon, label: &'static str, on: bool, after_active: bool, named: bool| {
            let tab = div()
                .id(id)
                .group(id)
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h_full()
                .map(|el| if named { el.px_3() } else { el.w(px(TAB_ICON_WIDTH)).justify_center() })
                .cursor_pointer()
                .text_color(if on { theme.text } else { theme.text_muted })
                .when(on, |el| el.font_weight(FontWeight::MEDIUM));
            tab_shape(tab, on, after_active, false, id.into(), edge, &theme)
                .child(icon_el(icon, px(14.), if on { theme.accent.into() } else { theme.text_dim.into() }))
                .when(named, |el| el.child(div().when(!on, |el| el.group_hover(id, |s| s.text_color(theme.text))).child(label)))
        };
        div()
            .id("sidebar-tabs")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .items_end()
            .pt(px(tab_strip_top(&theme)))
            // Clear of the island's rounded corners, like the editor's tabs.
            .pl(px(tab_strip_inset(&theme)))
            .pr(px(tab_strip_inset(&theme)))
            .overflow_hidden()
            .on_mouse_down(MouseButton::Right, cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                ws.open_sidebar_tab_menu(e.position, window, cx);
            }))
            .child(
                tab("sidebar-files", Icon::Folder, "Files", files_on, false, files_named)
                    .tooltip(|window, cx| status_tooltip("Files", &ToggleSidebar, window, cx))
                    .on_click(cx.listener(|ws, _, window, cx| ws.show_files(window, cx))),
            )
            .child(
                tab("sidebar-commit", Icon::GitBranch, "Commit", !files_on, files_on, commit_named)
                    // Uncommitted changes, so they show from the Files view too (the
                    // tooltip has them when the tab is only its icon).
                    .when_some(count.clone().filter(|_| commit_named), |el, count| {
                        el.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(if files_on { theme.text_dim } else { theme.accent })
                                .child(count),
                        )
                    })
                    // The count as it is now, also while the tooltip is shown.
                    .tooltip(move |window, cx| {
                        let keys = binding_label(&ToggleCommit, window);
                        crate::ui::entity_tooltip_view(
                            &workspace,
                            move |ws, _| {
                                let label = match ws.git_status.files.len() {
                                    0 => "Commit".to_string(),
                                    changes => format!("Commit ({changes} changed)"),
                                };
                                with_keys(&label, keys.as_deref())
                            },
                            cx,
                        )
                    })
                    .on_click(cx.listener(|ws, _, window, cx| ws.show_commit(&ShowCommit, window, cx))),
            )
            // Left-aligned on either side, like the editor's tabs; the free
            // space after them drags the window.
            .child(super::title_bar::window_drag_area("sidebar-tabs-free").flex_1().min_w_0())
    }
}
