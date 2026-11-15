//! Preferences in a window of its own: one for the whole app, since the
//! settings are (`settings.json`), so it can stay open beside the code and a
//! change shows in every project window as it's made. Opening it again brings
//! it forward on the page asked for. It reopens where it was last, on the
//! page shown last; files it opens go to the project window used last.

use gpui::{
    Action, App, AppContext, Context, Entity, FocusHandle, Focusable, Global, IntoElement, ParentElement, Render,
    Styled, Subscription, Task, TitlebarOptions, Window, WindowOptions, div, point, prelude::*, px, size,
};

use super::{Page, Preferences, PreferencesEvent};
use crate::{
    assets::Icon,
    state::SessionState,
    theme::Theme,
    ui::{chrome_bg, icon_button, icon_el, popover},
    workspace::{self, CloseTab, CloseWindow, OpenKeymap, OpenSettingsFile, TITLEBAR_HEIGHT, Workspace},
};

/// The page Preferences shows while its window is open, else `None`; the
/// title bar's Settings and Extensions buttons light up for it.
#[derive(Default)]
pub struct Shown(pub Option<Page>);

impl Global for Shown {}

/// The page shown when the window last closed, to reopen there.
struct LastPage(Page);

impl Global for LastPage {}

/// Opens Preferences (on `page`, else where it was last), or brings the open
/// window forward and shows `page` there.
pub fn open(page: Option<Page>, cx: &mut App) {
    if let Some(existing) = cx.windows().into_iter().find_map(|w| w.downcast::<PreferencesWindow>()) {
        existing
            .update(cx, |root, window, cx| {
                if let Some(page) = page {
                    root.preferences.update(cx, |p, cx| p.show_page(page, window, cx));
                }
                window.activate_window();
            })
            .ok();
        return;
    }
    let page = page.or_else(|| cx.try_global::<LastPage>().map(|last| last.0)).unwrap_or(Page::Appearance);
    let min = size(px(720.), px(480.));
    let (bounds, display) = match SessionState::load().preferences_window {
        Some(saved) => workspace::placement_bounds(&saved, min, cx),
        // Over the project window it's opened from, as the Push and Merge windows are.
        None => {
            let first = size(px(940.), px(640.));
            match crate::window_frame::centered_over_active(first, cx) {
                Some((bounds, display)) => (gpui::WindowBounds::Windowed(bounds), display),
                None => (gpui::WindowBounds::Windowed(gpui::Bounds::centered(None, first, cx)), None),
            }
        }
    };
    let options = WindowOptions {
        window_bounds: Some(bounds),
        display_id: display,
        titlebar: Some(TitlebarOptions {
            title: Some("Settings".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(16.), px(13.))),
        }),
        window_min_size: Some(min),
        app_id: Some("dev.kuana.editor".into()),
        window_decorations: crate::window_frame::decorations(),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        crate::window_frame::opened(window, cx);
        cx.new(|cx| PreferencesWindow::new(page, window, cx))
    });
    if let Err(err) = opened {
        eprintln!("couldn't open Preferences: {err}");
    }
}

pub struct PreferencesWindow {
    preferences: Entity<Preferences>,
    /// Problems from the last change, shown until dismissed or the next one.
    problems: Vec<String>,
    /// Saves where the window is a moment after a move or resize settles.
    placement_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PreferencesWindow {
    fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let preferences = cx.new(|cx| Preferences::new(page, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&preferences, window, |this, _, event: &PreferencesEvent, window, cx| match event {
                PreferencesEvent::OpenSettingsFile => open_in_project_window(Box::new(OpenSettingsFile), cx),
                PreferencesEvent::OpenKeymapFile => open_in_project_window(Box::new(OpenKeymap), cx),
                PreferencesEvent::Problems(problems) => {
                    this.problems = problems.clone();
                    cx.notify();
                }
                PreferencesEvent::Dismissed => this.close(window, cx),
            }),
            // The title bar buttons follow the page shown.
            cx.observe(&preferences, |_, preferences, cx| set_shown(Some(preferences.read(cx).page()), cx)),
            cx.observe_window_bounds(window, |this, window, cx| this.remember_placement(window, cx)),
        ];
        // Text typed a moment ago still gets saved, however the window closes.
        cx.on_release(|this: &mut Self, cx| {
            let page = this.preferences.read(cx).page();
            this.preferences.update(cx, |p, cx| p.flush(cx));
            cx.set_global(LastPage(page));
            set_shown(None, cx);
        })
        .detach();
        set_shown(Some(page), cx);
        Self { preferences, problems: Vec::new(), placement_task: None, _subscriptions: subscriptions }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferences.update(cx, |p, cx| p.flush(cx));
        window.remove_window();
    }

    fn remember_placement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = workspace::placement_of(window, cx);
        SessionState::change_soon("preferences_window", 0, move |state| state.preferences_window = Some(placement.clone()));
        self.placement_task = Some(crate::state::write_soon(cx));
    }
}

fn set_shown(page: Option<Page>, cx: &mut App) {
    if cx.try_global::<Shown>().map(|s| s.0) != Some(page) {
        cx.set_global(Shown(page));
    }
}

/// Runs `action` (open `settings.json` / `keymap.json` in a tab) in the
/// project window used last, and brings it forward. Deferred: it updates
/// another window than the one whose event this is.
fn open_in_project_window(action: Box<dyn Action>, cx: &mut App) {
    cx.defer(move |cx| {
        let stacked = cx.window_stack().unwrap_or_default();
        let target = stacked
            .into_iter()
            .chain(cx.windows())
            .find_map(|w| w.downcast::<Workspace>());
        if let Some(target) = target {
            target
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    window.dispatch_action(action, cx);
                })
                .ok();
        }
    });
}

impl Focusable for PreferencesWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.preferences.focus_handle(cx)
    }
}

impl Render for PreferencesWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // A title bar of its own, like the project windows': the window
        // drags by it and (off macOS) has its buttons in it.
        let title_bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            .when(cfg!(windows), |el| el.occlude())
            .child(
                workspace::window_drag_area("preferences-title")
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child("Settings"),
            )
            .when(crate::window_frame::draws_controls(window), |el| el.child(workspace::window_controls(window, cx)));
        let problems = (!self.problems.is_empty()).then(|| {
            div().absolute().bottom(px(16.)).left_0().right_0().flex().justify_center().child(
                popover(&theme)
                    .flex()
                    .items_start()
                    .gap_2()
                    .max_w(px(560.))
                    .px_3()
                    .py_2()
                    .child(div().pt(px(1.)).child(icon_el(Icon::CircleX, px(14.), theme.error.into())))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            // Sized by its text: a zero basis (`flex_1`) in this
                            // shrink-wrapped popover wraps it a letter per line.
                            .flex_auto()
                            .min_w_0()
                            .gap_0p5()
                            .text_size(px(12.5))
                            .children(self.problems.iter().take(3).map(|p| div().text_color(theme.text).child(p.clone()))),
                    )
                    .child(
                        icon_button("preferences-problems-close", Icon::X, theme.text_dim.into(), cx)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.problems.clear();
                                cx.notify();
                            })),
                    ),
            )
        });
        div()
            .id("preferences-window")
            .key_context("PreferencesWindow")
            // ⌘W closes the window; it has no tabs. Escape is Preferences' own
            // (an open menu or a shortcut being recorded goes first).
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| this.close(window, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(chrome_bg(&theme, 0.6))
            .font_family(theme.ui_font.clone())
            .text_color(theme.text)
            .when(crate::window_frame::own_title_row(window), |el| el.child(title_bar))
            .child(div().flex_1().min_h_0().child(self.preferences.clone()))
            .children(problems)
            .children(crate::window_frame::resize_edges(window))
    }
}
