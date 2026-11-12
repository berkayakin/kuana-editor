//! The welcome screen shown without open files.

use super::*;

impl Workspace {
    /// A project's editor area with no file open: the quickest ways to open
    /// one, each clickable with its shortcut, like JetBrains' empty editor.
    pub(super) fn render_no_open_files(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let hint = |id: &'static str, label: &'static str, keys: Option<String>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_between()
                .gap_6()
                .w(px(300.))
                .h(px(28.))
                .cursor_pointer()
                .text_color(theme.text_muted)
                .hover(|s| s.text_color(theme.text))
                .child(label)
                .when_some(keys, |el, keys| el.child(kbd(keys, cx)))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(theme.background)
            .text_size(px(13.))
            .child(
                hint("empty-search", "Search Everywhere", Some(double_shift_label().to_string()))
                    .on_click(cx.listener(|ws, _, window, cx| ws.search_everywhere(&SearchEverywhere, window, cx))),
            )
            .child(
                hint("empty-go-to-file", "Go to File", binding_label(&GoToFile, window))
                    .on_click(cx.listener(|ws, _, window, cx| ws.go_to_file(&GoToFile, window, cx))),
            )
            .child(
                hint("empty-recent", "Recent Files", binding_label(&RecentFiles, window))
                    .on_click(cx.listener(|ws, _, window, cx| ws.recent_files(&RecentFiles, window, cx))),
            )
            .child(
                hint("empty-new-file", "New File", binding_label(&NewFile, window))
                    .on_click(cx.listener(|ws, _, window, cx| ws.new_file(&NewFile, window, cx))),
            )
            .child(div().mt_3().text_size(px(12.)).text_color(theme.text_dim).child("Drop files here to open them"))
    }

    pub(super) fn render_welcome(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Lay out for the space the welcome screen actually gets.
        let sidebar = if self.show_sidebar { self.shown_sidebar_width() } else { px(0.) };
        let available = window.viewport_size().width - sidebar;
        let wide = available > px(820.);

        // ---- Header ----
        let header = div()
            .flex()
            .items_center()
            .gap_4()
            .child(img(assets::LOGO).size(px(56.)).rounded(px(14.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    // Auto basis: a zero one (`flex_1`) can wrap the tagline a letter per line.
                    .flex_auto()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_2()
                            .child(div().font_family(assets::TITLE_FONT).text_size(px(32.)).line_height(px(36.)).text_color(theme.text).child("Kuana"))
                            .child(
                                div()
                                    .mb(px(5.))
                                    .px_1p5()
                                    .rounded(px(5.))
                                    .bg(theme.hover)
                                    .text_size(px(11.))
                                    .text_color(theme.text_dim)
                                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                            ),
                    )
                    .child(div().text_color(theme.text_dim).child("A fast, GPU-rendered code editor")),
            )
            .child(
                div()
                    .id("w-theme")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .px_2p5()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(theme.border_subtle)
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover_bg(theme.hover)
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(|ws, _, window, cx| ws.select_theme(&SelectTheme, window, cx)))
                    .tooltip(|window, cx| status_tooltip("Select Theme…", &SelectTheme, window, cx))
                    .child(icon_el(Icon::Palette, px(13.), theme.text_dim.into()))
                    .child(theme.name.clone()),
            );

        // ---- Start tiles ----
        // Hovered, a card lights up like a focused text field: the border eases
        // to the accent and the same soft glow fades in.
        let (edge, lit) = (Hsla::from(theme.border_subtle), Hsla::from(theme.accent).alpha(0.7));
        let tile = |id: &'static str, icon: Icon, title: &'static str, detail: &'static str, keys: Option<String>| {
            let glow = crate::ui::field_glow(&theme);
            div()
                .id(id)
                .flex()
                .flex_col()
                .gap_3()
                .p(px(14.))
                .rounded(px(12.))
                .bg(theme.panel)
                .border_1()
                .cursor_pointer()
                .active(|s| s.bg(theme.active))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        // Just the icon, in the accent: no tile behind it.
                        .child(icon_el(icon, px(24.), theme.accent.into()))
                        .when_some(keys, |el, k| el.child(kbd(k, cx))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(px(12.)).text_color(theme.text_dim).line_clamp(2).child(detail)),
                )
                .with_hover_transition(SharedString::from(format!("{id}-hover")), transition::FOCUS, move |el, t| {
                    el.border_color(crate::transition::mix(edge, lit, t)).when(t > 0., |el| el.shadow(crate::ui::faded(glow, t)))
                })
        };
        let tiles = div()
            .grid()
            .grid_cols(if wide { 4 } else { 2 })
            .gap_3()
            // Opening a project is what most people come here for: its card
            // comes first, and the file card says it's for files on their own.
            .child(
                tile("w-open-folder", Icon::FolderSearch, "Open Folder…", "Open a project with its file tree", binding_label(&OpenFolder, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_folder(&OpenFolder, w, cx))),
            )
            .child(
                tile("w-open-file", Icon::FileCode, "Open File…", "Edit files without a project", binding_label(&OpenFile, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_file(&OpenFile, w, cx))),
            )
            .child(
                tile("w-new-file", Icon::FilePlus, "New File", "Start from an empty buffer", binding_label(&NewFile, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.new_file(&NewFile, w, cx))),
            )
            .child(
                tile("w-clone", Icon::Download, "Clone Repository…", "From GitHub, GitLab or any Git URL", None)
                    .on_click(cx.listener(|ws, _, w, cx| ws.git_clone(&GitClone, w, cx))),
            );

        // ---- Recent projects (and this session's files) ----
        let mut recent = div().flex().flex_col().gap_2().child(section_label("Recent Projects", &theme));
        if self.recent_projects.is_empty() {
            recent = recent.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .py_6()
                    .rounded(px(12.))
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .child(icon_el(Icon::History, px(20.), theme.text_dim.into()))
                    .child(div().mt_1().text_color(theme.text_muted).child("No recent projects"))
                    .child(div().text_size(px(12.)).text_color(theme.text_dim).child("Folders you open or clone show up here")),
            );
        } else {
            let list = self.recent_projects.iter().enumerate().map(|(ix, path)| {
                let name = folder_name(path);
                let parent = path.parent().map(clone_dialog::display_path).unwrap_or_default();
                let current = self.root.as_deref() == Some(path.as_path());
                let missing = !path.exists();
                let group = SharedString::from(format!("recent-{ix}"));
                let color = monogram_color(&name, &theme);
                let open = path.clone();
                let remove = path.clone();
                div()
                    .id(("w-recent", ix))
                    .group(group.clone())
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(46.))
                    .px_2()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .row_hover(theme.hover)
                    .when(missing, |el| el.opacity(0.55))
                    .on_click(cx.listener(move |ws, ev: &ClickEvent, window, cx| {
                        ws.open_recent_project(open.clone(), ev.modifiers().secondary(), window, cx)
                    }))
                    .child(
                        div()
                            .size(px(30.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(color.alpha(0.16))
                            .text_color(color)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().truncate().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(name))
                                    .when(current, |el| {
                                        el.child(
                                            div()
                                                .px_1p5()
                                                .rounded(px(4.))
                                                .bg(theme.accent_soft)
                                                .text_size(px(10.5))
                                                .text_color(theme.accent)
                                                .child("Open"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme.text_dim)
                                    .child(if missing { format!("{parent} · missing") } else { parent }),
                            ),
                    )
                    .child(
                        icon_button(("w-recent-x", ix), Icon::X, theme.text_dim.into(), cx)
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .tooltip(|_, cx| tooltip_view("Remove from Recent Projects", cx))
                            .on_click(cx.listener(move |ws, _, _, cx| {
                                cx.stop_propagation();
                                ws.remove_recent_project(&remove, cx);
                            })),
                    )
            });
            recent = recent.child(card(&theme).p_1().children(list));
        }
        let files: Vec<PathBuf> = self.recent.iter().filter(|p| p.is_file()).take(5).cloned().collect();
        if !files.is_empty() {
            recent = recent.child(div().h(px(8.))).child(section_label("Recent Files", &theme)).child(
                card(&theme).p_1().children(files.into_iter().enumerate().map(|(ix, path)| {
                    let icon = file_icon(&path, px(15.), &theme);
                    let rel = self.display_rel(&path);
                    let open = path.clone();
                    div()
                        .id(("w-file", ix))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(30.))
                        .px_2()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .row_hover(theme.hover)
                        .on_click(cx.listener(move |ws, _, window, cx| ws.open_path(open.clone(), window, cx)))
                        .child(icon)
                        .child(div().truncate().text_color(theme.text_muted).child(rel))
                })),
            );
        }

        // ---- Git ----
        let git = self.render_welcome_git(window, cx);

        // ---- Quick actions ----
        let double_shift = double_shift_label();
        let auto_save_on = self.auto_save != AutoSave::Off;
        let quick = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_label("Quick Actions", &theme))
            .child(
                card(&theme)
                    .p_1()
                    .child(
                        welcome_row("w-q-search", Icon::Search, "Search Everywhere", Some(double_shift.into()), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.search_everywhere(&SearchEverywhere, w, cx))),
                    )
                    .child(
                        welcome_row("w-q-action", Icon::Command, "Find Action…", binding_label(&FindAction, window), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.find_action(&FindAction, w, cx))),
                    )
                    .when(self.root.is_some(), |el| {
                        el.child(
                            welcome_row("w-q-file", Icon::FileText, "Go to File…", binding_label(&GoToFile, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.go_to_file(&GoToFile, w, cx))),
                        )
                        .child(
                            welcome_row("w-q-find", Icon::TextSearch, "Find in Files…", binding_label(&FindInFiles, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.find_in_files(&FindInFiles, w, cx))),
                        )
                    })
                    .child(
                        welcome_row("w-q-theme", Icon::Palette, "Select Theme…", binding_label(&SelectTheme, window), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.select_theme(&SelectTheme, w, cx))),
                    )
                    .child(
                        welcome_row("w-q-autosave", Icon::Save, "Auto Save", None, &theme, cx)
                            .child(switch(auto_save_on, &theme))
                            .on_click(cx.listener(|ws, _, w, cx| ws.toggle_auto_save(&ToggleAutoSave, w, cx))),
                    ),
            );

        let left = div().flex().flex_col().gap_5().flex_1().min_w_0().child(recent).child(quick);
        let right = div()
            .flex()
            .flex_col()
            .gap_5()
            .when(wide, |el| el.w(px(320.)).flex_shrink_0())
            .child(git);

        let tip = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .text_size(px(12.))
            .text_color(theme.text_dim)
            .child(icon_el(Icon::Keyboard, px(13.), theme.text_dim.into()))
            .child("Press")
            .child(kbd(double_shift, cx))
            .child("to find files and actions, or")
            .children(binding_label(&FindAction, window).map(|k| kbd(k, cx)))
            .child("for any command");

        let page = div()
            .id("welcome")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.welcome_scroll)
            .bg(theme.background)
            .child(
                div()
                    .min_h(gpui::relative(1.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .px_6()
                    .py(px(40.))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(900.))
                            .flex()
                            .flex_col()
                            .gap(px(28.))
                            .child(header)
                            .child(tiles)
                            .child(
                                div()
                                    .flex()
                                    .gap_6()
                                    .when(wide, |el| el.flex_row().items_start())
                                    .when(!wide, |el| el.flex_col())
                                    .child(left)
                                    .child(right),
                            )
                            .child(tip),
                    ),
            );
        div().relative().size_full().child(page).child(crate::ui::scrollbar("welcome-scrollbar", &self.welcome_scroll, cx))
    }

    /// The welcome screen's Git card: repository state and actions, or a way
    /// to get a repository.
    pub(super) fn render_welcome_git(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let section = div().flex().flex_col().gap_2().child(section_label("Git", &theme));
        let body_text = |text: String| div().text_size(px(12.)).text_color(theme.text_dim).child(text);

        if !self.has_repo(cx) {
            let (title, detail) = match self.root.as_deref() {
                Some(root) => ("Not a Git repository", format!("Start tracking changes in {} with Git.", folder_name(root))),
                None => ("No repository open", "Clone a repository, or open a folder that's already under version control.".to_string()),
            };
            return section.child(
                card(&theme)
                    .p(px(14.))
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(icon_el(Icon::GitBranch, px(15.), theme.text_muted.into()))
                                    .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(title)),
                            )
                            .child(body_text(detail)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(self.root.is_some(), |el| {
                                el.child(self.primary_button("w-git-init", Icon::Plus, "Initialize", cx, |ws, _, window, cx| {
                                    ws.git_init(&GitInit, window, cx)
                                }))
                            })
                            .child(self.primary_button("w-git-clone", Icon::Download, "Clone…", cx, |ws, _, window, cx| {
                                ws.git_clone(&GitClone, window, cx)
                            })),
                    ),
            )
            .into_any_element();
        };

        let status = &self.git_status(cx);
        let branch = status.branch.clone().unwrap_or_else(|| "detached HEAD".into());
        let changes = status.files.len();
        let web = self.git.read(cx).remote().and_then(git::web_url);
        let counter = |icon: Icon, n: u32, tip: &'static str, id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_0p5()
                .px_1p5()
                .h(px(20.))
                .rounded(px(5.))
                .bg(theme.hover)
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(11.), theme.text_muted.into()))
                .child(n.to_string())
        };

        let summary = div()
            .flex()
            .flex_col()
            .gap_2()
            .p(px(14.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .id("w-git-branch")
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|ws, _, w, cx| ws.git_branches(&GitBranches, w, cx)))
                    .tooltip(|_, cx| tooltip_view("Switch or create a branch", cx))
                    .child(icon_el(Icon::GitBranch, px(15.), theme.accent.into()))
                    .child(div().flex_1().min_w_0().truncate().text_color(theme.text).font_weight(FontWeight::SEMIBOLD).child(branch))
                    .when(status.ahead > 0, |el| el.child(counter(Icon::ArrowUp, status.ahead, "Commits to push", "w-ahead")))
                    .when(status.behind > 0, |el| el.child(counter(Icon::ArrowDown, status.behind, "Commits to pull", "w-behind")))
                    .child(icon_el(Icon::ChevronDown, px(13.), theme.text_dim.into())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .map(|el| {
                        if changes == 0 {
                            el.text_color(theme.text_dim)
                                .child(icon_el(Icon::CircleCheck, px(13.), theme.success.into()))
                                .child("Working tree clean")
                        } else {
                            el.text_color(theme.text_muted)
                                .child(icon_el(Icon::CircleAlert, px(13.), theme.git_modified.into()))
                                .child(format!("{changes} changed file{}", if changes == 1 { "" } else { "s" }))
                        }
                    })
                    .when_some(self.git.read(cx).busy().label().cloned(), |el, busy| el.child(div().flex_1()).child(div().text_color(theme.accent).child(busy))),
            )
            .when_some(self.git.read(cx).last_commit().cloned(), |el, c| {
                el.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_size(px(12.))
                        .child(div().pt(px(1.)).child(icon_el(Icon::GitCommit, px(13.), theme.text_dim.into())))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(div().truncate().text_color(theme.text_muted).child(c.summary))
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(theme.text_dim)
                                        .child(format!("{} · {} · {}", c.sha, c.author, git::relative_time(c.time))),
                                ),
                        ),
                )
            })
            .when_some(web.clone(), |el, url| {
                let host = url.trim_start_matches("https://").to_string();
                el.child(
                    div()
                        .id("w-git-remote")
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(12.))
                        .text_color(theme.text_dim)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.accent))
                        .on_click(cx.listener(|ws, _, w, cx| ws.open_remote(&OpenRemote, w, cx)))
                        .tooltip(|_, cx| tooltip_view("Open in Browser", cx))
                        .child(icon_el(Icon::Link, px(13.), theme.text_dim.into()))
                        .child(div().flex_1().min_w_0().truncate().child(host))
                        .child(icon_el(Icon::ExternalLink, px(12.), theme.text_dim.into())),
                )
            });

        section
            .child(
            card(&theme)
                .child(summary)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .p_1()
                        .child(
                            welcome_row("w-git-commit", Icon::GitCommit, "Commit…", binding_label(&ShowCommit, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.show_commit(&ShowCommit, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-pull", Icon::ArrowDown, "Update Project", binding_label(&GitPull, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_pull(&GitPull, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-push", Icon::ArrowUp, "Push…", binding_label(&GitPush, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_push(&GitPush, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-fetch", Icon::Refresh, "Fetch", None, &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_fetch(&GitFetch, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-branches", Icon::GitBranch, "Branches…", None, &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_branches(&GitBranches, w, cx))),
                        ),
                ),
            )
            .into_any_element()
    }
}

/// Search Everywhere's shortcut (double Shift works in every keymap).
pub(super) fn double_shift_label() -> &'static str {
    if cfg!(target_os = "macos") { "⇧⇧" } else { "Shift Shift" }
}

/// A compact action row: icon, label and the action's shortcut.
pub(super) fn welcome_row(
    id: &'static str,
    icon: Icon,
    label: &'static str,
    keys: Option<String>,
    theme: &Theme,
    cx: &App,
) -> Button {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2p5()
        .h(px(32.))
        .px_2p5()
        .rounded(px(8.))
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover_bg(theme.hover)
        .hover(|s| s.text_color(theme.text))
        .active(|s| s.bg(theme.active))
        .child(icon_el(icon, px(15.), theme.text_dim.into()))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when_some(keys, |el, k| el.child(kbd(k, cx)))
}
