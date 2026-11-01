//! Kuana windows: opening, placement, quitting.

use super::*;

// ---- Windows ------------------------------------------------------------------------

/// Opens a Kuana window and runs `setup` on its workspace. With `near` (the
/// bounds of the window it was opened from) it's cascaded a little down and
/// to the right so it doesn't cover that window exactly.
/// Where modals start: a little below the title bar, lower in tall windows.
pub(super) fn modal_top(window: &Window) -> Pixels {
    (window.viewport_size().height * 0.12).max(px(TITLEBAR_HEIGHT + 12.))
}


/// The tallest a modal can be and still end above the window's bottom edge.
pub fn modal_max_height(window: &Window) -> Pixels {
    (window.viewport_size().height - modal_top(window) - px(16.)).max(px(120.))
}


pub fn open_window(
    near: Option<Bounds<Pixels>>,
    cx: &mut App,
    setup: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Option<WindowHandle<Workspace>> {
    let (bounds, display_id) = match near {
        Some(b) => (WindowBounds::Windowed(Bounds { origin: b.origin + point(px(28.), px(28.)), size: b.size }), None),
        None => saved_placement(cx)
            .unwrap_or_else(|| (WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx)), None)),
    };
    let options = WindowOptions {
        window_bounds: Some(bounds),
        display_id,
        titlebar: Some(TitlebarOptions {
            title: Some("Kuana".into()),
            appears_transparent: true,
            // On the line of the chips beside them (`title_bar::chip_top`).
            traffic_light_position: Some(point(px(16.), px(13.))),
        }),
        window_min_size: Some(size(px(480.), px(320.))),
        app_id: Some("dev.kuana.editor".into()),
        ..Default::default()
    };
    let window = match cx.open_window(options, |window, cx| cx.new(|cx| Workspace::new(window, cx))) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("couldn't open a window: {err}");
            return None;
        }
    };
    window
        .update(cx, |ws, window, cx| {
            setup(ws, window, cx);
            ws.window_ready(window, cx);
        })
        .ok();
    Some(window)
}


/// Where the last window was, if it was saved: on the same display when it's
/// still connected (kept fully on screen), else centered with the same size.
pub(super) fn saved_placement(cx: &App) -> Option<(WindowBounds, Option<gpui::DisplayId>)> {
    let saved = SessionState::load().window?;
    Some(placement_bounds(&saved, size(px(480.), px(320.)), cx))
}

/// Where a window is now, to save (`placement_bounds` puts it back).
pub fn placement_of(window: &Window, cx: &App) -> WindowPlacement {
    let (bounds, fullscreen) = match window.window_bounds() {
        WindowBounds::Fullscreen(bounds) => (bounds, true),
        WindowBounds::Windowed(bounds) | WindowBounds::Maximized(bounds) => (bounds, false),
    };
    WindowPlacement {
        display: window.display(cx).and_then(|d| d.uuid().ok()).map(|u| u.to_string()),
        x: bounds.origin.x.into(),
        y: bounds.origin.y.into(),
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
        fullscreen,
    }
}

/// A saved placement's bounds: on the same display when it's still connected
/// (kept fully on screen), else centered with the same size; never smaller
/// than `min`.
pub fn placement_bounds(saved: &WindowPlacement, min: gpui::Size<Pixels>, cx: &App) -> (WindowBounds, Option<gpui::DisplayId>) {
    let display = saved.display.as_ref().and_then(|uuid| {
        cx.displays().into_iter().find(|d| d.uuid().ok().is_some_and(|u| u.to_string() == *uuid))
    });
    let mut size = size(px(saved.width).max(min.width), px(saved.height).max(min.height));
    let bounds = match &display {
        Some(display) => {
            let area = display.bounds().size;
            size = size.min(&area);
            let x = saved.x.clamp(0., f32::from(area.width - size.width));
            let y = saved.y.clamp(0., f32::from(area.height - size.height));
            Bounds { origin: point(px(x), px(y)), size }
        }
        None => Bounds::centered(None, size, cx),
    };
    let bounds = if saved.fullscreen { WindowBounds::Fullscreen(bounds) } else { WindowBounds::Windowed(bounds) };
    (bounds, display.map(|d| d.id()))
}


/// The window that has `root` open as its project. The window that's
/// currently being updated can't be read, so callers check their own.
pub(super) fn window_with_project(root: &Path, cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .find(|w| w.read(cx).is_ok_and(|ws| ws.root.as_deref() == Some(root)))
}


/// Quits once every window has saved or discarded its edits. Windows with
/// unsaved changes ask in turn; their answer continues the quit. Must run
/// outside any window update (use `cx.defer`).
pub fn quit(cx: &mut App) {
    for handle in cx.windows() {
        let Some(workspace) = handle.downcast::<Workspace>() else { continue };
        let ready = workspace
            .update(cx, |ws, window, cx| {
                let ready = ws.prepare_to_close(CloseIntent::Quit, window, cx);
                if !ready {
                    window.activate_window();
                }
                ready
            })
            .unwrap_or(true);
        if !ready {
            return;
        }
    }
    cx.quit();
}


/// The user cancelled quitting, so every window guards its edits again.
pub(super) fn cancel_quit(cx: &mut App) {
    for handle in cx.windows() {
        if let Some(workspace) = handle.downcast::<Workspace>() {
            workspace.update(cx, |ws, _, _| ws.close_confirmed = false).ok();
        }
    }
}
