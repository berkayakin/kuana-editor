//! The title bar as a row of its own, for when `tools_location` puts the
//! tool buttons there instead of on the activity bar: the traffic lights (or
//! the app menu) and the project and branch at its start, the tools
//! (`Workspace::activity_tools`) at its end, groups between hairlines and the
//! less important ones in a More menu when the window is narrow. The editor's
//! and the sidebar's tabs are in the tab row under it (`sidebar_tabs.rs`).

use super::activity_bar::{Tool, fit_tools, more_entries, tools_in_title_bar, with_tool_tooltip};
use super::*;

/// A button (`bar_button`'s 30pt square) and the gap after it.
const BUTTON: f32 = 32.;
/// The line between groups with its margins.
const SEPARATOR: f32 = 13.;
/// The row's free space kept for moving the window, at the least.
const DRAG_ROOM: f32 = 80.;
/// Off macOS, the window's buttons (`render_window_controls`).
const CONTROLS: f32 = 3. * 46.;

impl Workspace {
    /// Whether the window has the title bar row: the tools are in it, and
    /// Welcome Setup (which lays out its own top) isn't showing.
    pub(super) fn has_title_row(&self, cx: &App) -> bool {
        tools_in_title_bar(cx) && self.onboarding.is_none()
    }

    /// The title bar row, as tall as the tab row it stands in for, so the
    /// traffic lights keep their place. Its free parts move the window.
    pub(super) fn render_title_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mac = cfg!(target_os = "macos");
        let lead = if mac && !window.is_fullscreen() { 82. } else { GAP + 4. };
        let controls = if mac { GAP + 2. } else { CONTROLS };
        let free = f32::from(self.viewport.width) - lead - controls - DRAG_ROOM;
        // The project and branch may take half the free room; the tools the rest.
        let chips_room = (free / 2.).max(0.);
        let chips_width = self.chip_fit(chips_room, false, window, cx).map_or(0., |(_, width)| width);
        div()
            .id("title-row")
            .flex()
            .flex_shrink_0()
            .items_center()
            .w_full()
            .h(px(TAB_STRIP_HEIGHT - 1.))
            .child(window_drag_area("title-row-lead").flex_shrink_0().w(px(lead)))
            .when(app_menu::APP_MENU_IN_TITLE_BAR, |el| el.child(self.render_app_menu(cx)).child(div().flex_shrink_0().w(px(GAP))))
            .children(self.render_title_chips(chips_room, false, window, cx))
            .child(window_drag_area("title-row-free").flex_1().min_w(px(GAP)))
            .child(self.render_title_tools(free - chips_width, cx))
            .map(|el| match mac {
                true => el.child(window_drag_area("title-row-end").flex_shrink_0().w(px(controls))),
                false => el.child(div().w(px(GAP)).h_full().flex_shrink_0()).child(self.render_window_controls(window, cx)),
            })
    }

    /// The tools as icon buttons in `room`: an open panel or page in the
    /// accent, Run and Stop in their own colors.
    fn render_title_tools(&self, room: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let tools = self.activity_tools(cx);
        // The groups' separators, about as many as show.
        let shown = fit_tools(&tools, room - 2. * SEPARATOR, BUTTON);
        let hidden: Vec<usize> = (0..tools.len()).filter(|ix| !shown.contains(ix)).collect();
        let mut row: Vec<AnyElement> = Vec::new();
        let mut last_group = None;
        for &ix in &shown {
            let tool = &tools[ix];
            if last_group.is_some_and(|group| group != tool.group) {
                row.push(separator(&theme));
            }
            last_group = Some(tool.group);
            row.push(self.title_tool_button(tool, &theme, cx));
        }
        if !hidden.is_empty() {
            let entries = more_entries(&tools, &hidden);
            let lit = hidden.iter().any(|&ix| tools[ix].on);
            let (color, hover) = if lit { (theme.accent.into(), None) } else { (theme.text_dim.into(), Some(theme.text.into())) };
            row.push(
                self.bar_button("title-more", Icon::More, color, hover, cx, move |ws, event, window, cx| {
                    ws.show_more_tools(&entries, event.position(), window, cx)
                })
                .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(BUTTON - 30.))
            // Over the title bar's drag area (Windows): see `render_title_lead`.
            .when(cfg!(windows), |el| el.occlude())
            .children(row)
    }

    fn title_tool_button(&self, tool: &Tool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (color, hover): (Hsla, Option<Hsla>) = match tool.tint {
            _ if tool.on => (theme.accent.into(), None),
            // Faded a little at rest (`bar_button`), full on hover.
            Some(tint) => (tint, None),
            None => (theme.text_dim.into(), Some(theme.text.into())),
        };
        let run = tool.run;
        let el = self.bar_button(tool.id, tool.icon, color, hover, cx, move |ws, _, window, cx| run(ws, window, cx));
        with_tool_tooltip(el, tool, cx).into_any_element()
    }
}

/// The hairline between two groups of tools.
fn separator(theme: &Theme) -> AnyElement {
    div().flex_shrink_0().w(px(1.)).h(px(16.)).mx(px((SEPARATOR - 1.) / 2. - 1.)).bg(theme.border).into_any_element()
}
