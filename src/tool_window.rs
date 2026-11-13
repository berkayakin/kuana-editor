//! Views that open in a window of their own over the project window they
//! belong to: the Push dialog and the Merge view. Unlike a modal, the
//! project window stays usable beside them (a diff from the Push dialog
//! opens there; a file can be looked at while merging), and they can be
//! moved, resized or put on another display.
//!
//! The window is only a frame: a title bar to drag it by (with the window's
//! buttons off macOS) over the view, which keeps emitting its events to the
//! workspace that opened it (`Workspace::push_window`, `merge_window`).
//! That workspace closes it when it's done with it, when its project changes
//! and when it goes. It opens centered over the project window, at the size
//! it had last (`state.json`'s `push_window_size`, `merge_window_size`).

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels, Render, SharedString, Size,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point,
    prelude::*, px, size,
};

use crate::{
    state::SessionState,
    theme::Theme,
    ui::chrome_bg,
    workspace::{self, CloseTab, CloseWindow, TITLEBAR_HEIGHT},
};

/// Which view a window holds: its sizes and where its size is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Push,
    Merge,
}

impl Kind {
    fn min_size(self) -> Size<Pixels> {
        match self {
            Kind::Push => size(px(600.), px(360.)),
            Kind::Merge => size(px(720.), px(420.)),
        }
    }

    /// The size it opens at the first time: the Push dialog's own; the
    /// Merge view nearly the project window's.
    fn first_size(self, parent: Size<Pixels>) -> Size<Pixels> {
        match self {
            Kind::Push => size(px(920.), px(560.)),
            Kind::Merge => size(parent.width - px(64.), parent.height - px(64.)),
        }
    }

    fn saved_size(self) -> Option<Size<Pixels>> {
        let state = SessionState::load();
        let (w, h) = match self {
            Kind::Push => state.push_window_size,
            Kind::Merge => state.merge_window_size,
        }?;
        Some(size(px(w), px(h)))
    }

    fn save_size(self, shown: Size<Pixels>) {
        let shown = (f32::from(shown.width), f32::from(shown.height));
        match self {
            Kind::Push => SessionState::change_soon("push_window_size", 0, move |s| s.push_window_size = Some(shown)),
            Kind::Merge => SessionState::change_soon("merge_window_size", 0, move |s| s.merge_window_size = Some(shown)),
        }
    }
}

/// Opens `build`'s view in a window of its own over `parent`, titled
/// `title`. The view is made in the new window, so what it sets up for a
/// window (focus, editors' listeners) is for that one.
pub fn open<V: Render + Focusable>(
    kind: Kind,
    title: impl Into<SharedString>,
    parent: &Window,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> Option<WindowHandle<ToolWindow<V>>> {
    let title = title.into();
    let parent_bounds = parent.bounds();
    let min = kind.min_size();
    let wanted = kind.saved_size().unwrap_or_else(|| kind.first_size(parent_bounds.size));
    // Within the display it opens on, whatever it was last.
    let display = parent.display(cx);
    let room = display.as_ref().map(|d| d.bounds().size).unwrap_or(parent_bounds.size);
    let shown = size(
        wanted.width.max(min.width).min(room.width),
        wanted.height.max(min.height).min(room.height - px(TITLEBAR_HEIGHT)),
    );
    let mut origin = point(
        parent_bounds.origin.x + (parent_bounds.size.width - shown.width) / 2.,
        parent_bounds.origin.y + (parent_bounds.size.height - shown.height) / 2.,
    );
    if let Some(screen) = display.as_ref().map(|d| d.bounds()) {
        origin.x = origin.x.max(screen.origin.x).min(screen.origin.x + screen.size.width - shown.width);
        origin.y = origin.y.max(screen.origin.y).min(screen.origin.y + screen.size.height - shown.height);
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds { origin, size: shown })),
        display_id: display.map(|d| d.id()),
        titlebar: Some(TitlebarOptions { title: Some(title.clone()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(13.))) }),
        // Over its project window where the system knows such a thing (X11, Wayland).
        kind: WindowKind::Floating,
        window_min_size: Some(min),
        is_minimizable: false,
        app_id: Some("dev.kuana.editor".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        let view = build(window, cx);
        cx.new(|cx| ToolWindow::new(kind, title, view, window, cx))
    });
    match opened {
        Ok(handle) => Some(handle),
        Err(err) => {
            eprintln!("couldn't open a window: {err}");
            None
        }
    }
}

/// Closes the window, if it's still open. Deferred: it may be asked from
/// an update of another window, or of this one.
pub fn close<V: Render + Focusable>(handle: WindowHandle<ToolWindow<V>>, cx: &mut App) {
    cx.defer(move |cx| {
        handle.update(cx, |_, window, _| window.remove_window()).ok();
    });
}

/// Brings the window forward; false when it's gone.
pub fn activate<V: Render + Focusable>(handle: &WindowHandle<ToolWindow<V>>, cx: &mut App) -> bool {
    handle.update(cx, |_, window, _| window.activate_window()).is_ok()
}

/// The view a window shows, while the window is open.
pub fn view<V: Render + Focusable>(handle: &WindowHandle<ToolWindow<V>>, cx: &App) -> Option<Entity<V>> {
    handle.read(cx).ok().map(|w| w.view.clone())
}

/// The window around a view: its title bar over it.
pub struct ToolWindow<V> {
    title: SharedString,
    view: Entity<V>,
    /// Saves the window's size a moment after a resize settles.
    size_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl<V: Render + Focusable> ToolWindow<V> {
    fn new(kind: Kind, title: SharedString, view: Entity<V>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.focus(&view.focus_handle(cx));
        let mut last = window.bounds().size;
        let subscriptions = vec![cx.observe_window_bounds(window, move |this, window, cx| {
            // Moves aren't kept: it opens over its project window each time.
            let now = window.bounds().size;
            if now != last && !window.is_maximized() && !window.is_fullscreen() {
                last = now;
                kind.save_size(now);
                this.size_task = Some(crate::state::write_soon(cx));
            }
        })];
        Self { title, view, size_task: None, _subscriptions: subscriptions }
    }
}

impl<V: Render + Focusable> Focusable for ToolWindow<V> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl<V: Render + Focusable> Render for ToolWindow<V> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Like the project windows' and Settings': the window drags by it
        // and (off macOS) has its buttons in it.
        let title_bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            .when(cfg!(windows), |el| el.occlude())
            .child(
                workspace::window_drag_area("tool-window-title")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Clear of the traffic lights on both sides, so the title stays centered.
                    .px(px(84.))
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child(div().min_w_0().truncate().child(self.title.clone())),
            )
            .when(!cfg!(target_os = "macos"), |el| el.child(workspace::window_controls(window, cx)));
        div()
            .id("tool-window")
            // ⌘W closes the window; it has no tabs.
            .on_action(cx.listener(|_, _: &CloseTab, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .size_full()
            .flex()
            .flex_col()
            .bg(chrome_bg(&theme, 0.6))
            .font_family(theme.ui_font.clone())
            .text_size(px(theme.ui_font_size))
            .text_color(theme.text)
            .child(title_bar)
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}
