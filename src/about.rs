//! About Kuana: a small window of its own, so it also opens when no project
//! window is. Shows the logo, the name in the title font, the version
//! and build, with a button that copies them for bug reports.

use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, IntoElement, Render, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, img, point, prelude::*, px, size,
};

use crate::{
    assets,
    editor::Cancel,
    theme::Theme,
    ui::{chrome_bg, soft_shadow, text_button},
    workspace::{CloseTab, CloseWindow},
};

actions!(kuana, [About]);

/// "0.1.0 (987a960)": the commit comes from `build.rs` and is left out when
/// Kuana wasn't built from a git checkout.
pub(crate) fn version() -> String {
    match option_env!("KUANA_COMMIT") {
        Some(commit) => format!("{} ({commit})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Opens the About window, or brings the open one to the front.
pub fn open(cx: &mut App) {
    if let Some(existing) = cx.windows().into_iter().find_map(|w| w.downcast::<AboutWindow>()) {
        existing.update(cx, |_, window, _| window.activate_window()).ok();
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(300.), px(330.)), cx))),
        titlebar: Some(TitlebarOptions {
            title: Some("About Kuana".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(12.))),
        }),
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("dev.kuana.editor".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        crate::window_frame::opened(window, cx);
        cx.new(|cx| AboutWindow::new(window, cx))
    });
    if let Err(err) = opened {
        eprintln!("couldn't open the About window: {err}");
    }
}

pub struct AboutWindow {
    focus_handle: FocusHandle,
    /// Shows "Copied" on the button for a moment after copying.
    copied: Option<Task<()>>,
}

impl AboutWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self { focus_handle, copied: None }
    }

    fn copy_version(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(format!("Kuana {}", version())));
        self.copied = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Focusable for AboutWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .key_context("About")
            .track_focus(&self.focus_handle)
            // ⌘W closes this window like any other; it has no tabs.
            .on_action(cx.listener(|_, _: &CloseTab, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|_, _: &Cancel, window, _| window.remove_window()))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(44.))
            .pb_5()
            .px_5()
            .bg(chrome_bg(&theme, 0.6))
            .font_family(theme.ui_font.clone())
            .text_size(px(theme.ui_font_size))
            .text_color(theme.text)
            .child(img(assets::LOGO).size(px(72.)).rounded(px(18.)).shadow(soft_shadow(&theme, 2)))
            .child(
                div()
                    .mt_3()
                    .font_family(assets::TITLE_FONT)
                    .text_size(px(36.))
                    .line_height(px(42.))
                    .child("Kuana"),
            )
            .child(div().text_color(theme.text_muted).child(format!("Version {}", version())))
            .child(div().mt_1().text_color(theme.text_dim).child("A fast, GPU-rendered code editor"))
            .child(
                div().mt_4().child(
                    text_button("about-copy", if self.copied.is_some() { "Copied" } else { "Copy Version" }, false, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.copy_version(cx))),
                ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_dim)
                    .child(format!("© 2026 Kuana contributors · {} license", env!("CARGO_PKG_LICENSE").replace(" OR ", " or "))),
            )
    }
}
