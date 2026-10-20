//! The app's menus where the system has no menu bar for them (Windows,
//! Linux): a ☰ at the title bar's left opens File and shows the other
//! menus' names beside it, like JetBrains'. They're the menus
//! `main.rs` sets (`cx.get_menus()`), each shown as a context menu; while
//! one is open, pointing at or clicking another name, or ←/→, opens that.

use super::*;
use gpui::{OwnedMenuItem, canvas, deferred};

/// macOS shows the menus in its own menu bar.
pub(super) const APP_MENU_IN_TITLE_BAR: bool = cfg!(not(target_os = "macos"));

impl Workspace {
    /// Opens menu `ix` (`cx.get_menus()` order), in place of any open menu.
    pub(super) fn open_app_menu(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = cx.get_menus().and_then(|menus| menus.into_iter().nth(ix)) else { return };
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        add_items(menu.items, &mut entries, &mut actions, window);
        // Placed under its name by `render_app_menu`, not at a position.
        self.show_context_menu(entries, actions, point(px(0.), px(0.)), None, window, cx);
        if let Some(open) = self.context_menu.as_mut() {
            open.app_menu = Some(ix);
        }
    }

    /// ←/→ in an open title bar menu: the menu next to it, wrapping around.
    pub(super) fn step_app_menu(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.context_menu.as_ref().and_then(|m| m.app_menu) else { return };
        let count = cx.get_menus().map_or(0, |m| m.len());
        if count > 0 {
            self.open_app_menu(if forward { (ix + 1) % count } else { (ix + count - 1) % count }, window, cx);
        }
    }

    /// Whether one of the title bar's menus is open (its names are shown).
    pub(super) fn app_menu_open(&self) -> bool {
        self.context_menu.as_ref().is_some_and(|m| m.app_menu.is_some())
    }

    /// The menu whose name is at `position`, while one of them is open (the
    /// open menu's overlay covers the names, so it asks).
    pub(super) fn app_menu_at(&self, position: gpui::Point<Pixels>) -> Option<usize> {
        self.context_menu.as_ref()?.app_menu?;
        self.app_menu_names.borrow().iter().position(|b| b.contains(&position))
    }

    /// The ☰, and the menus' names while one is open, with that menu under its name.
    pub(super) fn render_app_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let open = self.context_menu.as_ref().and_then(|m| Some((m.app_menu?, m.menu.clone())));
        let names: Vec<SharedString> = match open {
            Some(_) => cx.get_menus().unwrap_or_default().into_iter().map(|m| m.name).collect(),
            None => Vec::new(),
        };
        let bounds = self.app_menu_names.clone();
        bounds.borrow_mut().clear();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(2.))
            .child(
                self.bar_button(
                    "app-menu",
                    Icon::Menu,
                    if open.is_some() { theme.accent } else { theme.text_dim }.into(),
                    (open.is_none()).then(|| theme.text.into()),
                    cx,
                    |ws, _, window, cx| ws.open_app_menu(0, window, cx),
                )
                // Open, it would cover the menus' names.
                .when(open.is_none(), |el| el.tooltip(|_, cx| tooltip_view("Menu", cx))),
            )
            .children(names.into_iter().enumerate().map(|(ix, name)| {
                let shown = open.as_ref().filter(|(i, _)| *i == ix).map(|(_, menu)| menu.clone());
                let bounds = bounds.clone();
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .h(px(24.))
                    .px_2()
                    .rounded(px(6.))
                    .text_size(px(12.5))
                    .when(shown.is_some(), |el| el.bg(theme.hover))
                    .text_color(if shown.is_some() { theme.text } else { theme.text_muted })
                    .child(name)
                    .child(
                        canvas(
                            move |b, _, _| {
                                let mut names = bounds.borrow_mut();
                                if names.len() <= ix {
                                    names.resize(ix + 1, Bounds::default());
                                }
                                names[ix] = b;
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    // The menu itself, just below its name; `render_context_menu`
                    // draws only the overlay that closes it.
                    .when_some(shown, |el, menu| {
                        el.child(
                            div().absolute().left_0().top(px(28.)).child(deferred(
                                gpui::anchored().snap_to_window_with_margin(px(8.)).child(
                                    div()
                                        .occlude()
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                                        .child(menu),
                                ),
                            )),
                        )
                    })
            }))
    }
}

/// A menu's items as context menu entries: actions with their shortcuts,
/// separators, a submenu's items under its name.
fn add_items(items: Vec<OwnedMenuItem>, entries: &mut Vec<MenuEntry>, actions: &mut Vec<Option<MenuAction>>, window: &Window) {
    for item in items {
        match item {
            OwnedMenuItem::Action { name, action, .. } => {
                entries.push(MenuEntry::text(name).shortcut(binding_label(&*action, window)));
                actions.push(Some(MenuAction::Run(action)));
            }
            OwnedMenuItem::Separator => {
                entries.push(MenuEntry::Separator);
                actions.push(None);
            }
            OwnedMenuItem::Submenu(menu) => {
                entries.push(MenuEntry::Header(menu.name));
                actions.push(None);
                add_items(menu.items, entries, actions, window);
            }
            OwnedMenuItem::SystemMenu(_) => {}
        }
    }
}
