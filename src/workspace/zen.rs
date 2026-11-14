//! Zen mode (Toggle Zen Mode): everything but the editor goes (the activity
//! bar, the sidebars, the bottom panels, the status bar, the editor's tabs
//! and breadcrumbs, editors' minimaps and line numbers) and a lone pane's
//! text is centered, `zen_width` characters wide, full screen unless
//! `zen_full_screen` is off. What it hid is kept in `Zen` and put back when
//! it ends. Panels and the sidebar can still be opened meanwhile; what's
//! remembered for the next launch (`save_state`, the session, the window's
//! placement) is the layout from before, so quitting in Zen mode leaves
//! nothing hidden.

use super::*;
use crate::editor::ZenLook;

/// Room a centered column needs besides its text: the gutter (its fold
/// arrows, marks and diagnostic dot), the text's padding, the scrollbar and
/// the island's insets.
const ZEN_CHROME: f32 = 96.;

/// What Zen mode hid, put back when it ends.
pub(super) struct Zen {
    show_sidebar: bool,
    second_view: Option<SidebarMode>,
    bottom: Option<BottomPanel>,
    /// Zen mode made the window full screen, so leaving it undoes just that.
    full_screen: bool,
}

impl Workspace {
    pub(super) fn toggle_zen_mode(&mut self, _: &ToggleZenMode, window: &mut Window, cx: &mut Context<Self>) {
        match self.zen.take() {
            Some(zen) => self.leave_zen(zen, window, cx),
            None => self.enter_zen(window, cx),
        }
    }

    fn enter_zen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Welcome Setup has the whole window already.
        if self.onboarding.is_some() {
            return;
        }
        let full_screen = Settings::get(cx).zen_full_screen() && !window.is_fullscreen();
        self.zen = Some(Zen { show_sidebar: self.show_sidebar, second_view: self.second_view, bottom: self.bottom, full_screen });
        self.show_sidebar = false;
        self.second_view = None;
        self.bottom = None;
        self.context_menu = None;
        if full_screen {
            window.toggle_fullscreen();
        }
        self.sync_zen_editors(cx);
        // The keyboard may have been in a panel that's gone now.
        self.focus_active(window, cx);
        cx.notify();
    }

    fn leave_zen(&mut self, zen: Zen, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = zen.show_sidebar;
        self.second_view = zen.second_view;
        self.bottom = zen.bottom;
        // Left full screen meanwhile (the window's green button): nothing to undo.
        if zen.full_screen && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        self.sync_zen_editors(cx);
        self.save_state(cx);
        cx.notify();
    }

    /// How editors look now: Zen mode's look while it's on.
    pub(super) fn zen_look(&self, cx: &App) -> Option<ZenLook> {
        self.zen.as_ref().map(|_| ZenLook { line_numbers: Settings::get(cx).zen_line_numbers() })
    }

    /// Tells every editor whether Zen mode is on, and its options (they're
    /// settings, so they can change while it's on).
    pub(super) fn sync_zen_editors(&mut self, cx: &mut Context<Self>) {
        let look = self.zen_look(cx);
        for editor in self.editors.clone() {
            editor.update(cx, |e, cx| e.set_zen(look, cx));
        }
    }

    /// What to remember of the layout for the next launch (the sidebar, the
    /// second sidebar's view, the bottom panel): Zen mode's hiding is for
    /// this run only, so while it's on, the layout from before.
    pub(super) fn layout_to_save(&self) -> (bool, Option<SidebarMode>, Option<BottomPanel>) {
        match &self.zen {
            Some(zen) => (zen.show_sidebar, zen.second_view, zen.bottom),
            None => (self.show_sidebar, self.second_view, self.bottom),
        }
    }

    /// While the window is full screen for Zen mode, its placement isn't the
    /// one to open the next launch in.
    pub(super) fn zen_full_screen_now(&self, window: &Window) -> bool {
        self.zen.as_ref().is_some_and(|zen| zen.full_screen) && window.is_fullscreen()
    }

    /// The width Zen mode centers a lone pane at, in window pixels: `zen_width`
    /// characters of the editor's font, plus the gutter's line numbers when
    /// they stay and the rest of an editor's chrome.
    pub(super) fn zen_column_width(&self, window: &Window, cx: &App) -> f32 {
        let theme = Theme::get(cx);
        let settings = Settings::get(cx);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&gpui::font(theme.mono_font.clone()));
        let char_width = text_system
            .advance(font_id, px(theme.editor_font_size), 'm')
            .map(|s| f32::from(s.width))
            .unwrap_or(theme.editor_font_size * 0.6);
        // Line numbers take at least three digits' room (`Editor::layout_frame`); four here.
        let numbers = if settings.zen_line_numbers() { 4. * char_width } else { 0. };
        // A character's slack: the editor wraps a character short of its right edge.
        ((settings.zen_width() + 1.) * char_width + numbers + ZEN_CHROME).ceil()
    }

    /// In Zen mode, a pill at the window's top to leave it, shown while the
    /// pointer is over the top row (which keeps moving the window).
    pub(super) fn render_zen_exit(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let keys = binding_label(&ToggleZenMode, window);
        div()
            .id("zen-top")
            .group("zen-top")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT - 1.))
            .flex()
            .items_center()
            .justify_center()
            // The group's hitbox (a group has one) doesn't block the drag area under it.
            .child(
                crate::ui::popover(&theme)
                    .id("zen-exit")
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_3()
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .invisible()
                    .group_hover("zen-top", |s| s.visible())
                    // In front of the drag area, which mustn't take its press:
                    // it occludes the row there (hidden too), so it shows for
                    // its own hover as well as the row's.
                    .occlude()
                    .hover(|s| s.visible().text_color(theme.text))
                    .on_click(cx.listener(|ws, _, window, cx| ws.toggle_zen_mode(&ToggleZenMode, window, cx)))
                    .child("Exit Zen Mode")
                    .children(keys.map(|keys| kbd(keys, cx))),
            )
    }
}
