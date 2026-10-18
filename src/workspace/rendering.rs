//! Drawing the workspace: title bar, sidebar, panes, panels, status bar, modals.

use super::*;

impl Workspace {
    // ---- Rendering --------------------------------------------------------------

    pub(super) fn render_toasts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let theme = Theme::get(cx).clone();
        Some(
            div()
                .absolute()
                .right(px(16.))
                .bottom(px(STATUSBAR_HEIGHT + 12.))
                .flex()
                .flex_col()
                .gap_2()
                .w(px(360.))
                .children(self.toasts.iter().map(|t| {
                    let id = t.id;
                    // Slides up and fades in; leaving, the reverse.
                    let fade = move |el: Stateful<Div>, t: f32| el.opacity(t).relative().top(px((1. - t) * 12.));
                    let (icon, color): (Icon, Hsla) = match t.kind {
                        ToastKind::Success => (Icon::CircleCheck, theme.success.into()),
                        ToastKind::Info => (Icon::CircleAlert, theme.text_muted.into()),
                        ToastKind::Error => (Icon::CircleX, theme.error.into()),
                    };
                    div()
                        .id(("toast", id))
                        .flex()
                        .gap_2()
                        .p_3()
                        .rounded(px(12.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(Hsla::from(theme.border).alpha(0.85))
                        .shadow(soft_shadow(&theme, 2))
                        .cursor_pointer()
                        .on_click(cx.listener(move |ws, _, _, cx| ws.dismiss_toast(id, cx)))
                        .child(div().pt(px(1.)).child(icon_el(icon, px(16.), color)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                // Auto basis: a zero one (`flex_1`) can leave the text
                                // wrapped as if it had no room, a letter per line.
                                .flex_auto()
                                .min_w_0()
                                .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(t.message.clone()))
                                .when_some(t.detail.clone(), |el, d| {
                                    el.child(div().text_size(px(12.)).text_color(theme.text_muted).line_clamp(4).child(d))
                                }),
                        )
                        // Only while what it does still applies.
                        .when_some(t.button.filter(|button| self.toast_button_applies(*button, cx)), |el, button| {
                            el.child(
                                crate::ui::text_button(("toast-button", id), button.label(), false, cx)
                                    .h(px(24.))
                                    .px_2p5()
                                    .text_size(px(12.))
                                    .on_click(cx.listener(move |ws, _, window, cx| {
                                        cx.stop_propagation();
                                        ws.dismiss_toast(id, cx);
                                        ws.run_toast_button(button, window, cx);
                                    })),
                            )
                        })
                        .with_transition(gpui::ElementId::NamedInteger("toast-fade".into(), id as u64), !t.leaving, transition::TOAST, fade)
                        .appear()
                }))
                .into_any_element(),
        )
    }

    /// An invisible grab area filling the gap after an island; only the
    /// cursor shows it.
    pub(super) fn resize_handle(&self, edge: Edge, cx: &mut Context<Self>) -> impl IntoElement {
        let id: gpui::SharedString = match edge {
            Edge::Problems => "resize-problems".into(),
            Edge::Sidebar => "resize-sidebar".into(),
            Edge::Pane(ix) => format!("resize-pane-{ix}").into(),
        };
        let handle = div().id(id).absolute();
        let handle = match edge {
            Edge::Problems => handle.top(px(-GAP - 1.)).left_0().right_0().h(px(GAP)).cursor(CursorStyle::ResizeUpDown),
            // In the gap between the sidebar and the editor, whichever side it's on.
            Edge::Sidebar if self.sidebar_on_right(cx) => {
                handle.top_0().bottom_0().left(px(-GAP - 1.)).w(px(GAP)).cursor(CursorStyle::ResizeLeftRight)
            }
            Edge::Sidebar => handle.top_0().bottom_0().right(px(-GAP - 1.)).w(px(GAP)).cursor(CursorStyle::ResizeLeftRight),
            // In the gap after the pane's slot: beside its island (the strip's
            // room above stays the window's drag area), or across under it.
            Edge::Pane(_) if self.pane_axis == PaneAxis::Row => handle
                .top(px(TAB_STRIP_HEIGHT - 1.))
                .bottom_0()
                .right(px(-GAP))
                .w(px(GAP))
                .cursor(CursorStyle::ResizeLeftRight),
            Edge::Pane(_) => handle.left_0().right_0().bottom(px(-GAP)).h(px(GAP)).cursor(CursorStyle::ResizeUpDown),
        };
        handle.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let (start, size) = match edge {
                    Edge::Problems => (e.position.y, ws.problems_height),
                    Edge::Sidebar => (e.position.x, ws.sidebar_width),
                    Edge::Pane(ix) => {
                        let bounds = ws.panes_bounds.get();
                        ws.pane_drag_pair = ws.pane_sizes[ix] + ws.pane_sizes.get(ix + 1).copied().unwrap_or(0.);
                        match ws.pane_axis {
                            PaneAxis::Row => (e.position.x, bounds.size.width * ws.pane_sizes[ix]),
                            PaneAxis::Column => (e.position.y, bounds.size.height * ws.pane_sizes[ix]),
                        }
                    }
                };
                ws.resizing = Some(Resize { edge, start, size });
                cx.notify();
            }),
        )
    }

    pub(super) fn primary_button(
        &self,
        id: &'static str,
        icon: Icon,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(30.))
            .px_3()
            .rounded(px(15.))
            .border_1()
            .border_color(Hsla::from(theme.accent).alpha(0.3))
            .text_color(theme.accent)
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover_fill(theme.accent_soft, Hsla::from(theme.accent).alpha(0.24))
            .on_click(cx.listener(on_click))
            .child(icon_el(icon, px(15.), theme.accent.into()))
            .child(label)
    }

    /// Tells the tree how its rows look: Git changes, what Git ignores, the
    /// open file, a menu's row. Also called where the Git data changes, not
    /// only from `render`: the tree is a cached view, and one told during
    /// another view's render isn't drawn again (a new file kept no color).
    pub(super) fn decorate_tree(&mut self, cx: &mut Context<Self>) {
        let active_path = self.active_item(cx).and_then(|t| t.path(cx));
        let menu_target = self.context_menu.as_ref().and_then(|m| m.target.clone());
        let (git_paths, git_ignored) = (self.git_paths.clone(), self.git_ignored.clone());
        self.tree.update(cx, |t, cx| t.decorate(git_paths, git_ignored, active_path, menu_target, cx));
    }

    /// While a resize is on, follows the pointer anywhere in the window and
    /// keeps the resize cursor. The root's own mouse-move listener only hears
    /// moves while it's hovered, which the Git diff (it occludes) prevents,
    /// and an element's cursor loses to the editor's or the diff's under it.
    fn resize_tracker(&self, resize: Resize, cx: &Context<Self>) -> impl IntoElement {
        let cursor = match resize.edge {
            Edge::Problems => CursorStyle::ResizeUpDown,
            Edge::Sidebar => CursorStyle::ResizeLeftRight,
            Edge::Pane(_) if self.pane_axis == PaneAxis::Row => CursorStyle::ResizeLeftRight,
            Edge::Pane(_) => CursorStyle::ResizeUpDown,
        };
        let this = cx.entity().downgrade();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.set_window_cursor_style(cursor);
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        this.update(cx, |ws, cx| ws.on_resize_move(event, window, cx)).ok();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }

    fn on_resize_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(resize) = self.resizing else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.resizing = None;
            cx.notify();
            return;
        }
        let viewport = window.viewport_size();
        match resize.edge {
            Edge::Problems => {
                // Keep room for the tab bar and a few editor lines above the panel.
                let max = (viewport.height - px(TITLEBAR_HEIGHT + TAB_HEIGHT + STATUSBAR_HEIGHT + 120.)).max(px(90.));
                self.problems_height = (resize.size + (resize.start - event.position.y)).clamp(px(90.), max);
            }
            Edge::Sidebar => {
                // Leave the editor at least 320pt wide. On the right, dragging
                // left (toward the editor) widens it.
                let max = (viewport.width - px(320.)).max(px(SIDEBAR_MIN_WIDTH));
                let delta = event.position.x - resize.start;
                let delta = if self.sidebar_on_right(cx) { -delta } else { delta };
                self.sidebar_width = (resize.size + delta).clamp(px(SIDEBAR_MIN_WIDTH), max);
            }
            Edge::Pane(ix) if ix + 1 < self.pane_sizes.len() => {
                let bounds = self.panes_bounds.get();
                let (total, delta) = match self.pane_axis {
                    PaneAxis::Row => (bounds.size.width, event.position.x - resize.start),
                    PaneAxis::Column => (bounds.size.height, event.position.y - resize.start),
                };
                if total > px(0.) {
                    let pair = self.pane_drag_pair;
                    // Every pane keeps at least a tenth of the space.
                    let share = ((resize.size + delta) / total).clamp(0.1, (pair - 0.1).max(0.1));
                    self.pane_sizes[ix] = share;
                    self.pane_sizes[ix + 1] = pair - share;
                }
            }
            Edge::Pane(_) => {}
        }
        cx.notify();
    }

    /// Forgets where a drag would drop. Called when a drag ends any way at
    /// all, since GPUI doesn't redraw by itself when one is cancelled.
    pub(super) fn clear_drag_state(&mut self, cx: &mut Context<Self>) {
        // The drop overlays are drag_over styles: they only need a redraw.
        self.tree.update(cx, |t, cx| t.set_drop_target(None, cx));
        for pane in &self.panes {
            pane.update(cx, |p, cx| p.set_file_drop(false, cx));
        }
        self.tree_root_drop = false;
        cx.notify();
    }

    pub(super) fn on_root_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.tree.read(cx).has_drop_target() || self.panes.iter().any(|p| p.read(cx).has_file_drop()) {
            self.clear_drag_state(cx);
        }
        if self.resizing.take().is_some() {
            self.save_state(cx);
            self.schedule_session_save(cx);
            cx.notify();
        }
    }

}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The Problems panel lists the active file's diagnostics.
        if self.shows(BottomPanel::Problems) {
            let active = self.active_editor(cx);
            self.problems.update(cx, |p, cx| p.set_editor(active, cx));
        }
        self.load_shown_tabs(cx);
        self.update_window_title(window, cx);
        // A drag that ended without a drop (Escape, left the window) leaves no highlight.
        if !cx.has_active_drag() && self.tree.read(cx).has_drop_target() {
            self.tree.update(cx, |t, cx| t.set_drop_target(None, cx));
        }
        self.decorate_tree(cx);
        let git_target = self.context_menu.as_ref().and_then(|m| m.git_target.clone());
        self.git_panel.update(cx, |p, cx| p.set_menu_target(git_target, cx));
        let theme = Theme::get(cx).clone();
        let modal = self.render_modal(window, cx);
        let toasts = self.render_toasts(cx);
        let terminal_panel = self.shows(BottomPanel::Terminal).then(|| self.render_terminal_panel(cx).into_any_element());
        let panes = self.render_panes(window, cx).into_any_element();

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::recent_tab))
            .on_action(cx.listener(Self::recent_tab_back))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_sidebar_position))
            .on_action(cx.listener(Self::toggle_problems))
            .on_action(cx.listener(Self::toggle_results))
            .on_action(cx.listener(Self::rerun_failed_tests))
            .on_action(cx.listener(Self::show_local_history))
            .on_action(cx.listener(Self::next_result))
            .on_action(cx.listener(Self::previous_result))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::replace))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::find_in_files))
            .on_action(cx.listener(Self::replace_in_files))
            .on_action(cx.listener(Self::search_everywhere))
            .on_action(cx.listener(Self::go_to_file))
            .on_action(cx.listener(Self::recent_files))
            .on_action(cx.listener(Self::find_action))
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::file_structure))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_settings_file))
            .on_action(cx.listener(Self::open_project_settings))
            .on_action(cx.listener(Self::restart_language_server))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::rerun))
            .on_action(cx.listener(Self::stop_run))
            .on_action(cx.listener(Self::run_configuration))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::next_pane))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::toggle_pin_tab))
            .on_action(cx.listener(Self::toggle_markdown_preview))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::close_terminal))
            .on_action(cx.listener(Self::open_keymap))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::open_themes_folder))
            .on_action(cx.listener(Self::open_snippets_folder))
            .on_action(cx.listener(Self::reload_themes))
            .on_action(cx.listener(Self::show_commit))
            .on_action(cx.listener(Self::toggle_commit))
            .on_action(cx.listener(Self::git_push))
            .on_action(cx.listener(Self::git_pull))
            .on_action(cx.listener(Self::git_fetch))
            .on_action(cx.listener(Self::git_branches))
            .on_action(cx.listener(Self::git_stash))
            .on_action(cx.listener(Self::manage_remotes))
            .on_action(cx.listener(Self::toggle_git_log))
            .on_action(cx.listener(Self::show_file_history))
            .on_action(cx.listener(Self::git_clone))
            .on_action(cx.listener(Self::git_init))
            .on_action(cx.listener(Self::open_remote))
            .on_action(cx.listener(Self::toggle_auto_save))
            .on_action(cx.listener(Self::check_for_updates))
            .on_action(cx.listener(Self::welcome_setup))
            .on_action(cx.listener(Self::go_to_symbol))
            .on_action(cx.listener(Self::split_terminal))
            .on_action(cx.listener(Self::rename_terminal))
            .on_action(cx.listener(Self::select_opened_file))
            .on_action(cx.listener(Self::debug_configuration))
            .on_action(cx.listener(Self::attach_to_process))
            .on_action(cx.listener(Self::add_conditional_breakpoint))
            .on_action(cx.listener(Self::add_logpoint))
            .on_action(cx.listener(Self::open_extensions))
            .on_action(cx.listener(Self::project_trust))
            .on_action(cx.listener(Self::vim_command_line))
            .on_action(cx.listener(Self::reload_extensions))
            .on_action(cx.listener(Self::resume))
            .on_action(cx.listener(Self::pause_program))
            .on_action(cx.listener(Self::step_over))
            .on_action(cx.listener(Self::step_into))
            .on_action(cx.listener(Self::step_out))
            .on_action(cx.listener(Self::toggle_debug_panel))
            .on_action(cx.listener(Self::install_shell_command))
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::minimize_window))
            .on_action(cx.listener(Self::zoom_window))
            .on_action(cx.listener(Self::open_folder_in_this_window))
            .on_action(cx.listener(Self::recent_projects))
            .on_action(cx.listener(Self::cancel))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .capture_key_down(cx.listener(Self::on_any_key_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_root_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_root_mouse_up))
            .on_drop(cx.listener(|ws, paths: &gpui::ExternalPaths, window, cx| ws.open_dropped(paths.paths(), window, cx)))
            .when_some(self.resizing, |el, resize| el.child(self.resize_tracker(resize, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            // The base the islands float on, faintly lit with the accent from above.
            .bg(chrome_bg(&theme, 0.45))
            .text_color(theme.text)
            .font_family(theme.ui_font.clone())
            .text_size(px(theme.ui_font_size))
            .child(self.render_titlebar(window, cx))
            .when_some(self.onboarding.as_ref().map(|(o, _)| o.clone()), |el, onboarding| {
                el.child(
                    island(&theme)
                        .flex_1()
                        .min_h_0()
                        .mx(px(GAP))
                        .mb(px(GAP))
                        .child(div().size_full().overflow_hidden().child(onboarding)),
                )
            })
            .when(self.onboarding.is_none(), |el| el.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .px(px(GAP))
                    .gap(px(GAP))
                    // The sidebar keeps its full height on either side; the
                    // bottom panels stay under the editor area.
                    .when(self.show_sidebar && !self.sidebar_on_right(cx), |el| el.child(self.render_sidebar(window, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(GAP))
                            .child(div().flex_1().min_h_0().child(panes))
                            .when(self.shows(BottomPanel::Problems), |el| el.child(self.render_problems(cx)))
                            .when(self.shows(BottomPanel::Results), |el| el.child(self.render_results(cx)))
                            .when(self.shows(BottomPanel::Terminal), |el| el.child(terminal_panel.unwrap()))
                            .when(self.shows(BottomPanel::GitLog), |el| el.child(self.render_git_log_panel(cx)))
                            .when(self.shows(BottomPanel::Debug), |el| el.children(self.render_debug_panel(cx))),
                    )
                    .when(self.show_sidebar && self.sidebar_on_right(cx), |el| el.child(self.render_sidebar(window, cx))),
            )
            .child(self.render_statusbar(cx)))
            .children(toasts)
            .children(self.render_context_menu(cx))
            .children(modal)
            .child({
                // A Finder drag that leaves the window or is cancelled only sends
                // FileDropEvent::Exited; listen for it to drop the highlights.
                let this = cx.entity().downgrade();
                gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let this = this.clone();
                        window.on_mouse_event(move |event: &gpui::FileDropEvent, _, _, cx| {
                            if matches!(event, gpui::FileDropEvent::Exited) {
                                this.update(cx, |ws, cx| ws.clear_drag_state(cx)).ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_0()
            })
    }
}





