//! A right-click menu: icons, shortcut hints, separators and section
//! headers, navigable with the keyboard. Emits the chosen item's index.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement, Render,
    SharedString, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{Cancel, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    theme::Theme,
    ui::{HoverFill, icon_el},
};

pub enum MenuEntry {
    Item {
        label: SharedString,
        icon: Option<Icon>,
        shortcut: Option<SharedString>,
        danger: bool,
        /// A line under the label, smaller (what a choice means).
        detail: Option<SharedString>,
    },
    Header(SharedString),
    Separator,
}

impl MenuEntry {
    pub fn item(label: impl Into<SharedString>, icon: Icon) -> Self {
        MenuEntry::Item { label: label.into(), icon: Some(icon), shortcut: None, danger: false, detail: None }
    }

    /// An item without an icon (its column stays reserved for alignment).
    pub fn text(label: impl Into<SharedString>) -> Self {
        MenuEntry::Item { label: label.into(), icon: None, shortcut: None, danger: false, detail: None }
    }

    pub fn shortcut(mut self, keys: Option<String>) -> Self {
        if let MenuEntry::Item { shortcut, .. } = &mut self {
            *shortcut = keys.map(Into::into);
        }
        self
    }

    pub fn detail(mut self, text: Option<impl Into<SharedString>>) -> Self {
        if let MenuEntry::Item { detail, .. } = &mut self {
            *detail = text.map(Into::into);
        }
        self
    }

    pub fn danger(mut self) -> Self {
        if let MenuEntry::Item { danger, .. } = &mut self {
            *danger = true;
        }
        self
    }

    fn is_item(&self) -> bool {
        matches!(self, MenuEntry::Item { .. })
    }
}

pub enum ContextMenuEvent {
    /// Index into the entries passed to [`ContextMenu::new`].
    Chose(usize),
    Dismissed,
    /// ← (false) or → (true): a menu bar's menu opens its neighbor.
    Neighbor(bool),
}

pub struct ContextMenu {
    entries: Vec<MenuEntry>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
}

impl EventEmitter<ContextMenuEvent> for ContextMenu {}

/// The menus made, while they're open (`menu_open`).
#[derive(Default)]
struct OpenMenus(Vec<gpui::WeakEntity<ContextMenu>>);

impl gpui::Global for OpenMenus {}

/// Whether a context menu is open. Tooltips built meanwhile are empty: GPUI
/// shows a tooltip by its element's bounds, even through the overlay a menu
/// lays over the window, so a button's would cover the menu it just opened
/// under the pointer.
pub fn menu_open(cx: &App) -> bool {
    cx.try_global::<OpenMenus>().is_some_and(|menus| menus.0.iter().any(|menu| menu.upgrade().is_some()))
}

impl ContextMenu {
    pub fn new(entries: Vec<MenuEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        // Closed, a menu is dropped: `menu_open` needs no word of it.
        let me = cx.weak_entity();
        cx.update_default_global::<OpenMenus, _>(|menus, _| {
            menus.0.retain(|menu| menu.upgrade().is_some());
            menus.0.push(me);
        });
        Self { entries, selected: None, focus_handle }
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let items: Vec<usize> = (0..self.entries.len()).filter(|&i| self.entries[i].is_item()).collect();
        if items.is_empty() {
            return;
        }
        let pos = self.selected.and_then(|s| items.iter().position(|&i| i == s));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => items.len() - 1,
            (Some(p), true) => (p + 1) % items.len(),
            (Some(p), false) => (p + items.len() - 1) % items.len(),
        };
        self.selected = Some(items[next]);
        cx.notify();
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            cx.emit(ContextMenuEvent::Chose(ix));
        }
    }

    fn on_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ContextMenuEvent::Neighbor(false));
    }

    fn on_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ContextMenuEvent::Neighbor(true));
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ContextMenuEvent::Dismissed);
    }
}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .key_context("ContextMenu")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_escape))
            .min_w(px(196.))
            .p_1()
            .flex()
            .flex_col()
            .bg(theme.elevated)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
            .rounded(px(10.))
            .shadow(crate::ui::soft_shadow(&theme, 2))
            .text_size(px(12.5))
            .children(self.entries.iter().enumerate().map(|(ix, entry)| match entry {
                MenuEntry::Separator => div().my(px(3.)).mx_1().h(px(1.)).bg(theme.border_subtle).into_any_element(),
                MenuEntry::Header(label) => div()
                    .px_2()
                    .pt_1()
                    .pb(px(2.))
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_dim)
                    .child(label.to_uppercase())
                    .into_any_element(),
                MenuEntry::Item { label, icon, shortcut, danger, detail } => {
                    let selected = self.selected == Some(ix);
                    let color: Hsla = if *danger { theme.error.into() } else { theme.text.into() };
                    // The whole row (not just its text) is the hover and click target.
                    // Rows rely on the column's stretch alignment: inside a popover
                    // the menu's width isn't known up front, so `w_full()` would
                    // collapse to the label's width.
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .id(ix)
                                .relative()
                                .flex()
                                .gap(px(7.))
                                // With a line under the label, the icon (a check) goes by the label.
                                .map(|el| if detail.is_some() { el.items_start().min_h(px(24.)).py(px(4.)) } else { el.items_center().h(px(24.)) })
                                .px(px(7.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(color)
                                .when(selected, |el| el.bg(theme.accent_soft))
                                .row_hover(theme.accent_soft)
                                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                    if this.selected != Some(ix) {
                                        this.selected = Some(ix);
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ContextMenuEvent::Chose(ix))))
                                .child(div().w(px(14.)).flex_shrink_0().when(detail.is_some(), |el| el.h(px(16.)).flex().items_center()).when_some(*icon, |el, icon| {
                                    el.child(icon_el(icon, px(13.), if *danger { color } else { theme.text_muted.into() }))
                                }))
                                .child(div().flex_1().flex().flex_col().child(label.clone()).when_some(detail.clone(), |el, detail| {
                                    el.child(div().max_w(px(300.)).text_size(px(11.5)).text_color(theme.text_dim).child(detail))
                                }))
                                .when_some(shortcut.clone(), |el, keys| {
                                    el.child(div().pl_4().text_size(px(12.)).text_color(theme.text_dim).child(keys))
                                }),
                        )
                        .into_any_element()
                }
            }))
    }
}
