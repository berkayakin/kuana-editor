//! The sidebar's views as tabs above its island, shaped like the editor's
//! (`tab_shape`) and in the same tab row, for when the tool buttons are in
//! the title bar's row (`tools_location`) and there's no activity bar to hold
//! them as vertical tabs (`activity_bar.rs`). As there, a view moved to the
//! second sidebar keeps its place among them as a button that shows or hides
//! it on its side.

use super::*;

/// A named tab's padding, icon and gap around its name (no close button).
const TAB_CHROME: f32 = 44.;
/// The Commit tab's change count: its gap and digits' size.
const COUNT_GAP: f32 = 6.;
const COUNT_SIZE: f32 = 11.5;
/// The menu of the views that don't fit: its button and the gap before it.
const MENU_WIDTH: f32 = 40.;

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

    /// Its icon in a menu, which takes Kuana's own icons only.
    pub(super) fn menu_icon(self) -> Icon {
        match self {
            SidebarMode::Files => Icon::Folder,
            SidebarMode::Commit => Icon::GitBranch,
            SidebarMode::Database => Icon::Database,
            SidebarMode::Extension(_) => Icon::Puzzle,
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

/// How a view shows in the strip.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ViewSlot {
    Named,
    Icon,
    /// In the menu of the views that don't fit.
    Menu,
}

/// How each view shows in `room`. `named` is a tab's width with its name;
/// none for a view moved to the second sidebar, whose button is always an
/// icon. Every name when they fit, else the current tab's only, else icons,
/// else as many icons as fit beside the menu that takes the rest: the first
/// ones, and the current tab whichever it is.
fn fit_views(named: &[Option<f32>], current: usize, room: f32) -> Vec<ViewSlot> {
    let count = named.len();
    let rest = |ix: usize| if named[ix].is_some() { ViewSlot::Named } else { ViewSlot::Icon };
    if named.iter().map(|width| width.unwrap_or(TAB_ICON_WIDTH)).sum::<f32>() <= room {
        return (0..count).map(rest).collect();
    }
    let others = TAB_ICON_WIDTH * count.saturating_sub(1) as f32;
    if named.get(current).copied().flatten().is_some_and(|width| width + others <= room) {
        return (0..count).map(|ix| if ix == current { ViewSlot::Named } else { ViewSlot::Icon }).collect();
    }
    if TAB_ICON_WIDTH * count as f32 <= room {
        return vec![ViewSlot::Icon; count];
    }
    let kept = (((room - MENU_WIDTH) / TAB_ICON_WIDTH).floor().max(1.)) as usize;
    // Past the kept ones, the current tab takes the last one's place.
    let first = if current < kept { kept } else { kept - 1 };
    (0..count).map(|ix| if ix < first || ix == current { ViewSlot::Icon } else { ViewSlot::Menu }).collect()
}

impl Workspace {
    /// The views as tabs on the chrome above the sidebar's island, as wide
    /// as it: the current one island-colored and joined to it, like the
    /// editor's current tab; the Commit tab counts uncommitted changes. A
    /// view moved to the second sidebar is an icon among them, lit while it
    /// shows there. Right-click moves a view or the sidebar to the other
    /// side, as on the activity bar.
    pub(super) fn render_sidebar_tabs(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut theme = Theme::get(cx).clone();
        // Lit with the tree's island while a drag targets the project folder.
        if self.tree_root_drop && self.sidebar_mode == SidebarMode::Files {
            theme.background = crate::ui::drop_lift(&theme).1.into();
        }
        let edge = crate::ui::island_edge(&theme);
        let changes = self.git_status(cx).files.len();
        let count = (changes > 0).then(|| if changes > 99 { "99+".to_string() } else { changes.to_string() });
        let modes = self.all_views();
        let mut names: Vec<String> = modes.iter().map(|m| m.label().to_string()).collect();
        names.push(count.clone().unwrap_or_default());
        let measured = measure_tab_names(&names, window, &theme);
        let named: Vec<Option<f32>> = modes
            .iter()
            .enumerate()
            .map(|(ix, &mode)| {
                let count_width = if mode == SidebarMode::Commit && count.is_some() { COUNT_GAP + measured[modes.len()].0 } else { 0. };
                (!self.in_second(mode)).then_some(TAB_CHROME + measured[ix].0 + count_width)
            })
            .collect();
        let current = modes.iter().position(|&m| m == self.sidebar_mode).unwrap_or(0);
        let width = f32::from(self.shown_sidebar_width());
        let inset = tab_strip_inset(&theme);
        let slots = fit_views(&named, current, width - 2. * inset);
        let shown: Vec<usize> = (0..modes.len()).filter(|&ix| slots[ix] != ViewSlot::Menu).collect();
        let in_menu: Vec<SidebarMode> = (0..modes.len()).filter(|&ix| slots[ix] == ViewSlot::Menu).map(|ix| modes[ix]).collect();
        // The one open on the other side (not in a window too narrow for it).
        let lit = self.second_shown().map(|(mode, _)| mode);
        let tabs = shown.iter().enumerate().map(|(at, &ix)| {
            let mode = modes[ix];
            let on = ix == current;
            let named = slots[ix] == ViewSlot::Named;
            let accent = on || lit == Some(mode);
            let after_current = at > 0 && shown[at - 1] == current;
            // Hairlines only between two tabs that sit on the chrome.
            let separator = !on && shown.get(at + 1).is_some_and(|&next| next != current);
            let group = SharedString::from(format!("sidebar-tab-{}", mode.saved_name().unwrap_or_else(|| mode.label().to_lowercase())));
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
            tab_shape(tab, on, after_current, separator, group.clone(), edge, &theme)
                .on_click(cx.listener(move |ws, _, window, cx| ws.view_tab_clicked(mode, window, cx)))
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
                            move |ws, cx| {
                                let label = match (mode, ws.git_status(cx).files.len()) {
                                    (SidebarMode::Commit, changes) if changes > 0 => format!("Commit ({changes} changed)"),
                                    _ => mode.label().to_string(),
                                };
                                with_keys(&label, keys.as_deref())
                            },
                            cx,
                        )
                    })
                })
                .child(gpui::svg().path(mode.icon_path()).size(px(14.)).flex_shrink_0().text_color(if accent { theme.accent } else { theme.text_dim }))
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
        // The views that don't fit, in a menu; in the accent while it holds
        // the one open on the other side.
        let menu = (!in_menu.is_empty()).then(|| {
            let count = in_menu.len();
            let holds_lit = lit.is_some_and(|mode| in_menu.contains(&mode));
            div()
                .id("sidebar-tabs-more")
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .gap_1()
                .w(px(MENU_WIDTH - 4.))
                .h(px(24.))
                .ml_1()
                .mb(px(strip_button_margin(&theme)))
                .rounded(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if holds_lit { theme.accent } else { theme.text_muted })
                .hover_bg(theme.hover)
                .when(!holds_lit, |el| el.hover(|s| s.text_color(theme.text)))
                .when(self.context_menu.is_none(), |el| el.tooltip(move |_, cx| tooltip_view(format!("{count} more view{}", plural(count)), cx)))
                .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| ws.open_more_views_menu(&in_menu, event.position(), window, cx)))
                .child("⋯")
                .child(count.to_string())
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
            .children(menu)
    }

    /// A click on a view's tab shows it; on the button of a view that's on
    /// the other side it also hides it again, like its activity bar button.
    fn view_tab_clicked(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_second(mode) {
            self.toggle_sidebar_view(mode, window, cx);
        } else {
            self.select_sidebar_view(mode, window, cx);
        }
    }

    /// The views a strip or header has no room to list, to pick from.
    pub(super) fn open_more_views_menu(&mut self, views: &[SidebarMode], position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let (mut entries, mut actions) = (Vec::new(), Vec::new());
        for &view in views {
            entries.push(MenuEntry::item(view.label(), view.menu_icon()));
            actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                ws.select_sidebar_view(view, window, cx)
            }))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
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

    /// All names when they fit, else the current tab's alone, else icons,
    /// else the ones that fit beside the menu, the current tab among them.
    #[test]
    fn tabs_give_up_their_names_when_narrow() {
        use ViewSlot::*;
        let named = [Some(70.), Some(80.), Some(90.)];
        assert_eq!(fit_views(&named, 1, 400.), vec![Named, Named, Named]);
        assert_eq!(fit_views(&named, 1, 170.), vec![Icon, Named, Icon]);
        assert_eq!(fit_views(&named, 1, 130.), vec![Icon, Icon, Icon]);
        assert_eq!(fit_views(&named, 1, 100.), vec![Menu, Icon, Menu]);
        assert_eq!(fit_views(&named, 2, 100.), vec![Menu, Menu, Icon]);
    }

    /// A view on the other side is an icon whatever the room, and counts as one.
    #[test]
    fn a_moved_view_keeps_an_icon() {
        use ViewSlot::*;
        let named = [Some(70.), Some(80.), None];
        assert_eq!(fit_views(&named, 0, 400.), vec![Named, Named, Icon]);
        assert_eq!(fit_views(&named, 0, 189.), vec![Named, Icon, Icon]);
        assert_eq!(fit_views(&named, 0, 149.), vec![Icon, Icon, Icon]);
    }

    /// Views past the strip's width go to the menu, never the current one.
    #[test]
    fn views_that_do_not_fit_go_to_the_menu() {
        use ViewSlot::*;
        let named = [Some(70.), Some(80.), None, Some(90.), Some(95.)];
        // The sidebar at its narrowest: two icons beside the menu.
        let room = SIDEBAR_MIN_WIDTH - 2. * TAB_STRIP_INSET;
        assert_eq!(fit_views(&named, 0, room), vec![Icon, Icon, Menu, Menu, Menu]);
        assert_eq!(fit_views(&named, 1, room), vec![Icon, Icon, Menu, Menu, Menu]);
        assert_eq!(fit_views(&named, 4, room), vec![Icon, Menu, Menu, Menu, Icon]);
        // What shows fits, with the menu.
        for current in 0..named.len() {
            let slots = fit_views(&named, current, room);
            let icons = slots.iter().filter(|slot| **slot == Icon).count() as f32;
            assert!(icons * TAB_ICON_WIDTH + MENU_WIDTH <= room, "{slots:?}");
            assert_eq!(slots[current], Icon);
        }
        // Less room than a tab and the menu: the current tab still shows.
        assert_eq!(fit_views(&named, 3, 50.), vec![Menu, Menu, Menu, Icon, Menu]);
    }
}
