//! The sidebar's views as tabs above its island, shaped like the editor's
//! (`tab_shape`) and in the same tab row, for when the tool buttons are in
//! the title bar's row (`tools_location`) and there's no activity bar to hold
//! them as vertical tabs (`activity_bar.rs`).

use super::*;

/// A named tab's padding, icon and gap around its name (no close button).
const TAB_CHROME: f32 = 44.;
/// The Commit tab's change count: its gap and digits' size.
const COUNT_GAP: f32 = 6.;
const COUNT_SIZE: f32 = 11.5;

impl SidebarMode {
    /// The views in their tabs' order.
    pub(super) const ALL: [SidebarMode; 3] = [SidebarMode::Files, SidebarMode::Commit, SidebarMode::Database];

    pub(super) fn label(self) -> SharedString {
        match self {
            SidebarMode::Files => "Files".into(),
            SidebarMode::Commit => "Commit".into(),
            SidebarMode::Database => "Database".into(),
            SidebarMode::Extension(_) => self.extension_look().map_or_else(|| "Extension".into(), |(title, _)| title),
        }
    }

    /// Its icon, as an asset path (an extension's view may bring its own).
    pub(super) fn icon_path(self) -> SharedString {
        match self {
            SidebarMode::Files => Icon::Folder.path().into(),
            SidebarMode::Commit => Icon::GitBranch.path().into(),
            SidebarMode::Database => Icon::Database.path().into(),
            SidebarMode::Extension(_) => self.extension_look().map_or_else(|| Icon::Puzzle.path().into(), |(_, icon)| icon),
        }
    }

    /// Its name in `state.json`; none for an extension's view that's gone.
    pub(super) fn saved_name(self) -> Option<String> {
        match self {
            SidebarMode::Files => Some("files".into()),
            SidebarMode::Commit => Some("commit".into()),
            SidebarMode::Database => Some("database".into()),
            SidebarMode::Extension(_) => self.extension_key().map(|key| format!("extension:{key}")),
        }
    }

    pub(super) fn from_saved(saved: &str) -> Option<SidebarMode> {
        match saved {
            "files" => Some(SidebarMode::Files),
            "commit" => Some(SidebarMode::Commit),
            "database" => Some(SidebarMode::Database),
            other => super::extension_ui::restore_extension_mode(other),
        }
    }

    /// The action that shows or hides this view, for its shortcut.
    fn toggle_action(self) -> Box<dyn gpui::Action> {
        match self {
            SidebarMode::Files => Box::new(ToggleSidebar),
            SidebarMode::Commit => Box::new(ToggleCommit),
            SidebarMode::Database => Box::new(ToggleDatabase),
            SidebarMode::Extension(_) => match self.extension_key() {
                Some(view) => Box::new(ToggleExtensionView { view }),
                None => Box::new(gpui::NoAction),
            },
        }
    }
}

/// Which tabs show their names in `room`, given each one's width named
/// (`named`) and which is current: all of them, else the current one only,
/// else none (their tooltips still name them).
fn fit_names(named: &[f32], current: usize, room: f32) -> Vec<bool> {
    if named.iter().sum::<f32>() <= room {
        return vec![true; named.len()];
    }
    let others = TAB_ICON_WIDTH * named.len().saturating_sub(1) as f32;
    let current_named = named[current] + others <= room;
    (0..named.len()).map(|ix| ix == current && current_named).collect()
}

impl Workspace {
    /// The views as tabs on the chrome above the sidebar's island, as wide
    /// as it: the current one island-colored and joined to it, like the
    /// editor's current tab; the Commit tab counts uncommitted changes.
    /// Right-click moves the sidebar to the other side, as on the activity bar.
    pub(super) fn render_sidebar_tabs(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut theme = Theme::get(cx).clone();
        // Lit with the tree's island while a drag targets the project folder.
        if self.tree_root_drop && self.sidebar_mode == SidebarMode::Files {
            theme.background = crate::ui::drop_lift(&theme).1.into();
        }
        let edge = crate::ui::island_edge(&theme);
        let changes = self.git_status.files.len();
        let count = (changes > 0).then(|| if changes > 99 { "99+".to_string() } else { changes.to_string() });
        // A view moved to the second sidebar isn't one of its tabs.
        let modes = self.sidebar_views();
        let mut names: Vec<String> = modes.iter().map(|m| m.label().to_string()).collect();
        names.push(count.clone().unwrap_or_default());
        let measured = measure_tab_names(&names, window, &theme);
        let named: Vec<f32> = modes
            .iter()
            .enumerate()
            .map(|(ix, &mode)| {
                let count_width = if mode == SidebarMode::Commit && count.is_some() { COUNT_GAP + measured[modes.len()].0 } else { 0. };
                TAB_CHROME + measured[ix].0 + count_width
            })
            .collect();
        let current = modes.iter().position(|&m| m == self.sidebar_mode).unwrap_or(0);
        let width = f32::from(self.shown_sidebar_width());
        let inset = tab_strip_inset(&theme);
        let shows_name = fit_names(&named, current, width - 2. * inset);
        let tabs = modes.iter().enumerate().map(|(ix, &mode)| {
            let on = ix == current;
            let named = shows_name[ix];
            // Hairlines only between two tabs that sit on the chrome.
            let separator = !on && ix + 1 < modes.len() && ix + 1 != current;
            let group = SharedString::from(format!("sidebar-tab-{}", mode.label().to_lowercase()));
            let workspace = cx.entity();
            let tab = div()
                .id(group.clone())
                .group(group.clone())
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
            tab_shape(tab, on, ix > 0 && ix - 1 == current, separator, group.clone(), edge, &theme)
                .on_click(cx.listener(move |ws, _, window, cx| ws.select_sidebar_view(mode, window, cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(move |ws, e: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    ws.open_view_menu(mode, e.position, window, cx);
                }))
                // Not while a menu is open: a right-click's opens under the
                // pointer, where the tooltip would cover it.
                .when(self.context_menu.is_none(), |el| {
                    el.tooltip(move |window, cx| {
                        let action = mode.toggle_action();
                        let keys = binding_label(&*action, window);
                        crate::ui::entity_tooltip_view(
                            &workspace,
                            move |ws, _| {
                                let label = match (mode, ws.git_status.files.len()) {
                                    (SidebarMode::Commit, changes) if changes > 0 => format!("Commit ({changes} changed)"),
                                    _ => mode.label().to_string(),
                                };
                                with_keys(&label, keys.as_deref())
                            },
                            cx,
                        )
                    })
                })
                .child(gpui::svg().path(mode.icon_path()).size(px(14.)).flex_shrink_0().text_color(if on { theme.accent } else { theme.text_dim }))
                .when(named, |el| el.child(div().when(!on, |el| el.group_hover(group.clone(), |s| s.text_color(theme.text))).child(mode.label())))
                .when_some(count.clone().filter(|_| mode == SidebarMode::Commit && named), |el, count| {
                    el.child(
                        div()
                            .text_size(px(COUNT_SIZE))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(if on { theme.accent } else { theme.text_dim })
                            .child(count),
                    )
                })
        });
        div()
            .id("sidebar-tabs")
            .w(px(width))
            .map(|el| tab_strip(el, &theme))
            .flex()
            .items_end()
            // Clear of the island's rounded corners, like the editor's tabs.
            .px(px(inset))
            .overflow_hidden()
            .on_mouse_down(MouseButton::Right, cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                ws.open_sidebar_tab_menu(e.position, window, cx);
            }))
            .children(tabs)
    }

    /// Shows `mode` in the sidebar; its tab doesn't hide the sidebar when
    /// it's current, unlike an activity bar view tab.
    pub(super) fn select_sidebar_view(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_shown(mode) {
            return;
        }
        // Each view's toggle shows it when it isn't the one showing.
        self.toggle_sidebar_view(mode, window, cx);
    }

    /// Shows `mode`, or hides the sidebar when it's the one showing.
    fn toggle_sidebar_view(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        match mode {
            SidebarMode::Files => self.toggle_sidebar(&ToggleSidebar, window, cx),
            SidebarMode::Commit => self.toggle_commit(&ToggleCommit, window, cx),
            SidebarMode::Database => self.toggle_database(&ToggleDatabase, window, cx),
            SidebarMode::Extension(_) => self.toggle_extension_mode(mode, window, cx),
        }
    }

    /// Shows or hides the sidebar on its current view (the title bar's
    /// sidebar button).
    pub(super) fn toggle_current_sidebar_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sidebar_view(self.sidebar_mode, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All names when they fit, else the current tab's alone, else none.
    #[test]
    fn tabs_give_up_their_names_when_narrow() {
        let named = [70., 80., 90.];
        assert_eq!(fit_names(&named, 1, 400.), vec![true, true, true]);
        assert_eq!(fit_names(&named, 1, 170.), vec![false, true, false]);
        assert_eq!(fit_names(&named, 1, 100.), vec![false, false, false]);
    }
}
