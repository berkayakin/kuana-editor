//! The parts of a window's frame the system may leave to Kuana. macOS draws
//! its own (the traffic lights); Windows has Kuana draw the buttons and tells
//! the system where they are (`WindowControlArea`). On Linux, Kuana asks for
//! client-side decorations: where it gets them (Wayland, X11 with a
//! compositor), it draws the buttons, moves the window from its free chrome
//! and resizes it from thin edges; where it doesn't (X11 without a
//! compositor), the window manager's title bar and frame do all that, and
//! Kuana draws no buttons of its own.

use std::time::Duration;

use gpui::{
    AnyElement, App, Bounds, CursorStyle, Decorations, IntoElement, MouseButton, ParentElement, Pixels, ResizeEdge, Styled, Window,
    WindowDecorations, div, prelude::*, px,
};

/// What a new window asks for: client-side decorations on Linux.
pub fn decorations() -> Option<WindowDecorations> {
    cfg!(target_os = "linux").then_some(WindowDecorations::Client)
}

/// Whether Kuana draws the window's buttons: on Windows always, on Linux
/// when the window got client-side decorations, never on macOS.
pub fn draws_controls(window: &Window) -> bool {
    if cfg!(target_os = "macos") {
        false
    } else if cfg!(windows) {
        true
    } else {
        matches!(window.window_decorations(), Decorations::Client { .. })
    }
}

/// Whether a window with no tabs along its top (Settings, the Push and Merge
/// windows, the connection dialog) draws a title row of its own: not on
/// Linux when the window manager's title bar is there, which says the same.
pub fn own_title_row(window: &Window) -> bool {
    !cfg!(target_os = "linux") || draws_controls(window)
}

/// Whether Kuana's free chrome moves the window itself (Linux; macOS and
/// Windows have their own ways, in `title_bar::window_drag_area`).
pub fn moves_window() -> bool {
    cfg!(any(target_os = "linux", target_os = "freebsd"))
}

/// How far in from the window's edge a press resizes it.
const EDGE: f32 = 5.;

/// Thin areas along the window's edges and corners that resize it, for a
/// window with client-side decorations; none when maximized or for edges
/// tiled against the screen's. Laid over the root's content, which must be
/// `relative`.
pub fn resize_edges(window: &Window) -> Option<AnyElement> {
    let Decorations::Client { tiling } = window.window_decorations() else { return None };
    if window.is_maximized() || window.is_fullscreen() {
        return None;
    }
    let edge = |id: &'static str, resize: ResizeEdge, cursor: CursorStyle| {
        div()
            .id(id)
            .absolute()
            .occlude()
            .cursor(cursor)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(resize);
            })
    };
    let e = px(EDGE);
    let corner = px(EDGE * 3.);
    let (top, bottom, left, right) = (!tiling.top, !tiling.bottom, !tiling.left, !tiling.right);
    let mut edges = Vec::new();
    if top {
        edges.push(edge("resize-top", ResizeEdge::Top, CursorStyle::ResizeUpDown).top_0().left_0().right_0().h(e).into_any_element());
    }
    if bottom {
        edges.push(edge("resize-bottom", ResizeEdge::Bottom, CursorStyle::ResizeUpDown).bottom_0().left_0().right_0().h(e).into_any_element());
    }
    if left {
        edges.push(edge("resize-left", ResizeEdge::Left, CursorStyle::ResizeLeftRight).top_0().bottom_0().left_0().w(e).into_any_element());
    }
    if right {
        edges.push(edge("resize-right", ResizeEdge::Right, CursorStyle::ResizeLeftRight).top_0().bottom_0().right_0().w(e).into_any_element());
    }
    // Corners after the edges, so they win where both are.
    if top && left {
        edges.push(edge("resize-top-left", ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight).top_0().left_0().size(corner).into_any_element());
    }
    if top && right {
        edges.push(edge("resize-top-right", ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft).top_0().right_0().size(corner).into_any_element());
    }
    if bottom && left {
        edges.push(edge("resize-bottom-left", ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft).bottom_0().left_0().size(corner).into_any_element());
    }
    if bottom && right {
        edges.push(edge("resize-bottom-right", ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight).bottom_0().right_0().size(corner).into_any_element());
    }
    Some(div().absolute().inset_0().children(edges).into_any_element())
}

/// Called as a window is made. On Linux it's drawn again over its first
/// seconds: GPUI draws a window's first frame before the X server shows it
/// and then only on a change, so without a window manager (whose reparenting
/// is one) it stayed black until the pointer crossed it.
pub fn opened(window: &Window, cx: &App) {
    if !cfg!(target_os = "linux") {
        return;
    }
    window
        .spawn(cx, async move |cx| {
            for delay in [100, 300, 1100, 2500] {
                cx.background_executor().timer(Duration::from_millis(delay)).await;
                if cx.update(|window, _| window.refresh()).is_err() {
                    return;
                }
            }
        })
        .detach();
}

/// `size` centered over the window that's active now (the project window
/// Settings was opened from), kept on its display; None without one.
pub fn centered_over_active(size: gpui::Size<Pixels>, cx: &mut App) -> Option<(Bounds<Pixels>, Option<gpui::DisplayId>)> {
    let active = cx.active_window()?;
    active
        .update(cx, |_, window, cx| {
            let parent = window.bounds();
            let display = window.display(cx);
            let mut origin = gpui::point(
                parent.origin.x + (parent.size.width - size.width) / 2.,
                parent.origin.y + (parent.size.height - size.height) / 2.,
            );
            if let Some(screen) = display.as_ref().map(|d| d.bounds()) {
                origin.x = origin.x.max(screen.origin.x).min(screen.origin.x + screen.size.width - size.width);
                origin.y = origin.y.max(screen.origin.y).min(screen.origin.y + screen.size.height - size.height);
            }
            (Bounds { origin, size }, display.map(|d| d.id()))
        })
        .ok()
}
