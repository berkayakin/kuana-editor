//! The title bar, which shares the window's top row with the editor's tabs:
//! its lead (traffic lights, project and branch chips) at the top left, its
//! trail (the run widget, window controls) at the top right, and the top
//! panes' tab strips between them (`PaneChrome::title_ends`). The tools are
//! on the activity bar (`activity_bar.rs`).

use super::*;
use gpui::svg;

/// Free space on the window's chrome (the title bar's empty parts, the room
/// after the top tab strips' tabs, the status bar's): drags the window, and a
/// double click zooms it. Only where nothing else is: a press reaches every
/// element under the pointer, so behind a tab it would move the window too.
/// Windows: the system does both over a drag area that no GPUI listener
/// handles, so it's occluded (the workspace root's focus-on-click would
/// count), which hides it from whatever is behind: drops that should land
/// there go on it too. macOS: a press hands the window to the window server.
pub fn window_drag_area(id: impl Into<gpui::ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .h_full()
        .window_control_area(WindowControlArea::Drag)
        .when(cfg!(windows), |el| el.occlude())
        .when(cfg!(target_os = "macos"), |el| {
            el.on_mouse_down(MouseButton::Left, |event, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    window.titlebar_double_click();
                } else {
                    #[cfg(target_os = "macos")]
                    drag_window();
                }
            })
        })
}

/// Moves the window with the pointer from a left mouse down being handled, on
/// top of whatever is there (a modifier-drag over tabs). macOS and Linux;
/// Windows has no call for it in GPUI.
pub fn move_window_with_pointer(window: &Window) {
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        drag_window();
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    window.start_window_move();
    #[cfg(target_os = "windows")]
    let _ = window;
}

/// Moves the key window with the pointer, from the mouse down being handled.
#[cfg(target_os = "macos")]
fn drag_window() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    let Some(app) = AnyClass::get(c"NSApplication") else { return };
    unsafe {
        let app: *mut AnyObject = msg_send![app, sharedApplication];
        let Some(app) = app.as_ref() else { return };
        let event: *mut AnyObject = msg_send![app, currentEvent];
        let Some(event) = event.as_ref() else { return };
        let window: *mut AnyObject = msg_send![event, window];
        if let Some(window) = window.as_ref() {
            let _: () = msg_send![window, performWindowDragWithEvent: event];
        }
    }
}

impl Workspace {
    /// The title bar's start, over the tab row's left end: room for the
    /// traffic lights (or the app menu). The project and branch are above the
    /// sidebar (`render_title_chips`), so they never push the tabs; with the
    /// sidebar hidden they come here, in the room they'd have there and with
    /// the branch and Restricted Mode as their icons, and the top left pane's
    /// tabs start after them.
    pub(super) fn render_title_lead(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        let theme = Theme::get(cx);
        div()
            .id("title-lead")
            .absolute()
            .top_0()
            .left_0()
            .h(px(TAB_STRIP_HEIGHT - 1.))
            .pt(px(chip_top(theme)))
            .flex()
            .flex_row()
            .items_center()
            // All of it moves the window but its buttons. On Windows a title bar
            // click must reach no listener, or GPUI reports it handled and the
            // system's drag and double-click don't happen: this area is occluded
            // (so the workspace root doesn't take focus on it), and the buttons
            // in front of it are too, so it isn't under them.
            .child(window_drag_area("titlebar-lead").absolute().top_0().left_0().size_full())
            .child(self.track_title_end(true, cx))
            // Room for the traffic lights, which macOS moves out of the window in full screen.
            .child(div().flex_shrink_0().w(px(if mac && !window.is_fullscreen() { 82. } else { GAP + 4. })))
            .when(app_menu::APP_MENU_IN_TITLE_BAR, |el| {
                el.child(self.render_app_menu(cx)).child(div().flex_shrink_0().w(px(GAP)))
            })
            .child(self.track_lead_base(window, cx))
            .when(!self.show_sidebar, |el| el.children(self.render_title_chips(self.lead_chips_room(cx), true, window, cx)))
    }

    /// The chips' room in the lead while the sidebar is hidden: what they'd
    /// have above it, so they show no more than with it.
    fn lead_chips_room(&self, cx: &App) -> f32 {
        // Before the lead was first drawn: about the traffic lights' room.
        let drawn = self.title_lead.get().base;
        let base = if drawn > 0. { drawn } else if cfg!(target_os = "macos") { 82. } else { GAP + 4. };
        self.sidebar_block_width(true, cx) - base - TITLE_END_GAP
    }

    /// Notes where the lead's chips start and how wide they'd be there (also
    /// while the sidebar shows), so the panes can be told where the lead
    /// ends in the frame the sidebar hides or shows (`title_ends_now`).
    fn track_lead_base(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let chips = self.chip_fit(self.lead_chips_room(cx), true, window, cx).map_or(0., |(_, width)| width);
        let lead = self.title_lead.clone();
        let sidebar = self.show_sidebar;
        gpui::canvas(|_, _, _| {}, move |bounds, _, _, _| lead.set(LeadLayout { base: f32::from(bounds.left()), chips, sidebar }))
            .flex_shrink_0()
            .w(px(0.))
            .h_full()
    }

    /// Where the title bar's ends are for the panes: as last drawn, but for
    /// the lead's end when the sidebar has shown or hidden since, which moves
    /// the chips in or out (else the tabs would sit under them for a frame).
    pub(super) fn title_ends_now(&self) -> (f32, f32) {
        let (lead_end, trail_start) = self.title_ends.get();
        let lead = self.title_lead.get();
        if lead.base > 0. && lead.sidebar != self.show_sidebar {
            (lead.base + if self.show_sidebar { 0. } else { lead.chips }, trail_start)
        } else {
            (lead_end, trail_start)
        }
    }

    /// The project, branch, commits to sync and Restricted Mode, in the tab
    /// row's part above the sidebar, `room` wide: fitted to it from their
    /// names' measured widths (`fit_chips`), so the editor's tabs start at their
    /// island whatever the names are; `icons` shows the branch and Restricted
    /// Mode as just their icons (their tooltips name them). `None` while an
    /// open app menu's names take their place.
    pub(super) fn render_title_chips(&self, room: f32, icons: bool, window: &Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (fit, _) = self.chip_fit(room, icons, window, cx)?;
        let counts = self.render_sync_counts(window, cx);
        Some(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(CHIP_GAP))
                // Over the title bar's drag area: see `render_title_lead`.
                .when(cfg!(windows), |el| el.occlude())
                .when_some(fit.project, |el, chars| el.child(self.render_project_chip(chars, fit.monogram, cx)))
                .when_some(fit.branch, |el, chars| el.child(self.render_branch_chip(chars, cx)))
                .when_some(counts.filter(|_| fit.counts), |el, counts| el.child(counts))
                .when_some(fit.restricted, |el, label| el.child(self.render_restricted_chip(label, cx))),
        )
    }

    /// How the chips fit `room` (see `render_title_chips`), and about how
    /// wide they are then. `None` while an open app menu's names take their place.
    pub(super) fn chip_fit(&self, room: f32, icons: bool, window: &Window, cx: &App) -> Option<(ChipFit, f32)> {
        if self.app_menu_open() {
            return None;
        }
        let theme = Theme::get(cx).clone();
        let size = px(13.);
        let measure = |text: &str, weight: FontWeight| -> Text {
            let font = gpui::Font { weight, ..gpui::font(theme.ui_font.clone()) };
            let run = gpui::TextRun { len: text.len(), font, color: theme.text.into(), background_color: None, underline: None, strikethrough: None };
            let width = window.text_system().shape_line(text.to_string().into(), size, &[run], None).width;
            Text { width: f32::from(width), chars: text.chars().count().max(1) }
        };
        let project = self.root.as_deref().map(|root| measure(&folder_name(root), FontWeight::SEMIBOLD));
        let branch = (self.repo.is_some() && !icons).then(|| measure(&self.branch_label(), FontWeight::NORMAL));
        let restricted = (self.restricted(cx) && !icons).then(|| RESTRICTED_LABEL_CHROME + measure("Restricted Mode", FontWeight::NORMAL).width);
        // As icons, their room is kept aside and the project fitted to the rest.
        let branch_icon = self.repo.is_some() && icons;
        let restricted_icon = self.restricted(cx) && icons;
        let room = room - [branch_icon, restricted_icon].iter().filter(|&&on| on).count() as f32 * (ICON_CHIP + CHIP_GAP);
        let counts = self.repo.is_some() && (self.git_status.ahead > 0 || self.git_status.behind > 0);
        let mut fit = fit_chips(ChipNeeds {
            room,
            project,
            branch,
            // About its icons and digits; it goes first anyway.
            counts: counts.then_some(SYNC_COUNTS_WIDTH),
            restricted,
            avatar: Settings::get(cx).project_avatar(),
        });
        if branch_icon {
            fit.branch = Some(None);
        }
        if restricted_icon {
            fit.restricted = Some(false);
        }
        // Each shown chip's chrome and its name as shown, with the gaps between.
        let name = |text: String, chars: usize, weight| measure(&ellipsize(&text, chars), weight).width;
        let project_chrome = if fit.monogram { PROJECT_CHROME } else { PROJECT_CHROME_PLAIN };
        let widths = [
            fit.project.map(|chars| match chars {
                Some(chars) => project_chrome + self.root.as_deref().map_or(0., |root| name(folder_name(root), chars, FontWeight::SEMIBOLD)),
                None => PROJECT_MONOGRAM,
            }),
            fit.branch.map(|chars| chars.map_or(ICON_CHIP, |chars| BRANCH_CHROME + name(self.branch_label(), chars, FontWeight::NORMAL))),
            fit.counts.then_some(SYNC_COUNTS_WIDTH),
            fit.restricted.map(|label| if label { restricted.unwrap_or(ICON_CHIP) } else { ICON_CHIP }),
        ];
        let shown: Vec<f32> = widths.into_iter().flatten().collect();
        let width = shown.iter().sum::<f32>() + CHIP_GAP * shown.len().saturating_sub(1) as f32;
        Some((fit, width))
    }

    /// The title bar's end, over the tab row's right end: off macOS, the
    /// window's buttons (Run and Debug are on the activity bar, or in the
    /// title bar's own row, `title_tools.rs`).
    pub(super) fn render_title_trail(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("title-trail")
            .absolute()
            .top_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT - 1.))
            .flex()
            .flex_row()
            .when(cfg!(windows), |el| el.occlude())
            .child(self.track_title_end(false, cx))
            .map(|el| match cfg!(target_os = "macos") {
                true => el.child(window_drag_area("titlebar-end").flex_shrink_0().w(px(GAP + 2.))),
                false => el.child(div().w(px(GAP)).h_full().flex_shrink_0()).child(self.render_window_controls(window, cx)),
            })
    }

    /// Notes where the lead ends (`lead`) or the trail starts, for the tab
    /// strips under them. A change tells the panes after this frame: they're
    /// cached, so told while it's drawn they wouldn't redraw.
    fn track_title_end(&self, lead: bool, cx: &Context<Self>) -> impl IntoElement + use<> {
        let ends = self.title_ends.clone();
        let this = cx.entity().downgrade();
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, _, cx| {
                let mut now = ends.get();
                if lead {
                    now.0 = f32::from(bounds.right());
                } else {
                    now.1 = f32::from(bounds.left());
                }
                if now != ends.get() {
                    ends.set(now);
                    cx.defer(move |cx| {
                        this.update(cx, |_, cx| cx.notify()).ok();
                    });
                }
            },
        )
        .absolute()
        .size_full()
    }

    /// Whether Settings is open, and if so whether on its Extensions page.
    pub(super) fn preferences_shown(&self, cx: &App) -> Option<bool> {
        let page = cx.try_global::<preferences::Shown>().and_then(|shown| shown.0)?;
        Some(page == preferences::Page::Extensions)
    }

    /// A title bar icon button: just the icon (18 px in a 30 px target), `color` at rest and `hover`
    /// under the mouse (`None` = the same color, a little brighter for being
    /// full strength: at rest it's slightly faded).
    pub(super) fn bar_button(
        &self,
        id: &'static str,
        icon: Icon,
        color: Hsla,
        hover: Option<Hsla>,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> crate::ui::Button {
        let (rest, hover) = match hover {
            Some(hover) => (color, hover),
            None => (color.alpha(color.a * 0.8), color),
        };
        div()
            .id(id)
            .size(px(30.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(on_click))
            // The icon eases to its hover color like the other buttons' fills
            // (an svg takes its own color, so it's added here, from the progress).
            .with_hover_transition(SharedString::from(format!("{id}-hover")), transition::FOCUS, move |el, t| {
                el.child(svg().path(icon.path()).size(px(18.)).flex_shrink_0().text_color(transition::mix(rest, hover, t)))
            })
    }

    /// The open project: a round tinted monogram (unless `project_avatar` is
    /// off) and the folder name, a pill like the branch chip beside it under
    /// the pointer. At rest it's plain text on the chrome: a filled pill would
    /// crowd the islands just below. Clicking lists recent projects.
    /// `chars`: the most of its name shown; `None`, just the monogram, which
    /// `avatar` shows (it's off in tight room and with `project_avatar` off).
    pub(super) fn render_project_chip(&self, chars: Option<usize>, avatar: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let name = self.root.as_deref().map(folder_name).unwrap_or_default();
        let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        let tint = monogram_color(&name, &theme);
        div()
            .id("project-chip")
            .flex()
            .items_center()
            .gap_2()
            .h(px(CHIP_HEIGHT))
            .pl(px(if avatar { 3. } else { 10. }))
            .pr(px(10.))
            .rounded_full()
            .cursor_pointer()
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .hover_fill(Hsla::from(theme.hover).alpha(0.), theme.hover)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.recent_projects(&RecentProjects, window, cx)))
            .tooltip(|window, cx| status_tooltip("Recent Projects…", &RecentProjects, window, cx))
            .when(chars.is_none(), |el| el.pr(px(3.)))
            .when(avatar, |el| el.child(
                div()
                    .size(px(CHIP_HEIGHT - 6.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .map(|el| {
                        if !theme.gradients {
                            el.bg(tint)
                        } else {
                            el.bg(gpui::linear_gradient(
                                135.,
                                gpui::linear_color_stop(tint, 0.),
                                gpui::linear_color_stop(tint.alpha(0.55), 1.),
                            ))
                        }
                    })
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(monogram_letter(tint, &theme))
                    .child(initial),
            ))
            .when_some(chars, |el, chars| el.child(div().whitespace_nowrap().child(ellipsize(&name, chars))))
    }

    /// The current branch beside the project name. Clicking opens Branches….
    /// `chars`: the most of its name shown; `None`, just its icon.
    pub(super) fn render_branch_chip(&self, chars: Option<usize>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let branch = self.branch_label();
        // Just the icon in the narrowest windows; the tooltip names the branch.
        let tip = if chars.is_some() { "Branches…".to_string() } else { format!("{branch} · Branches…") };
        // Uncommitted changes aren't marked here: the Commit tab counts them.
        div()
            .id("branch-chip")
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(CHIP_HEIGHT))
            .pl(px(10.))
            .pr(px(if chars.is_some() { 12. } else { 10. }))
            .rounded_full()
            .cursor_pointer()
            // The same size as the project's name beside it.
            .text_size(px(13.))
            .text_color(theme.text_muted)
            .hover_fill(Hsla::from(theme.hover).alpha(0.), theme.hover)
            .hover(|st| st.text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
            .tooltip(move |window, cx| status_tooltip(&tip, &GitBranches, window, cx))
            .child(icon_el(Icon::GitBranch, px(14.), theme.text_dim.into()))
            .when_some(chars, |el, chars| el.child(div().whitespace_nowrap().child(ellipsize(&branch, chars))))
    }

    /// Commits to push and to pull, beside the branch pill: Push… when there
    /// are some to push, else Update Project.
    pub(super) fn render_sync_counts(&self, window: &Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let s = &self.git_status;
        let (ahead, behind) = (s.ahead, s.behind);
        if self.repo.is_none() || (ahead == 0 && behind == 0) {
            return None;
        }
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let commits = |n: u32| if n == 1 { "1 commit".to_string() } else { format!("{n} commits") };
        let tip = match (ahead, behind) {
            (0, b) => format!("{} to pull", commits(b)),
            (a, 0) => format!("{} to push", commits(a)),
            (a, b) => format!("{} to push, {b} to pull", commits(a)),
        };
        // Behind and ahead: a push would be refused, so pull first, then push.
        let (label, action): (&str, &dyn gpui::Action) = match (ahead, behind) {
            (0, _) => ("Update Project", &GitPull),
            (_, 0) => ("Push…", &GitPush),
            _ => ("Sync: Update Project, then Push…", &GitSync),
        };
        let tip = match binding_label(action, window) {
            Some(keys) => format!("{tip}. {label} ({keys})"),
            None => format!("{tip}. {label}"),
        };
        let dim: Hsla = theme.text_dim.into();
        let bright: Hsla = theme.text.into();
        let count = move |icon: Icon, n: u32| {
            div().flex().items_center().gap(px(2.)).child(
                svg().path(icon.path()).size(px(14.)).flex_shrink_0().text_color(dim).group_hover("sync-counts", move |s| s.text_color(bright)),
            )
            .child(n.to_string())
        };
        Some(
            div()
                .id("sync-counts")
                .group("sync-counts")
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h(px(CHIP_HEIGHT))
                .px(px(8.))
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme.text_dim)
                .hover_bg(theme.hover)
                .hover(|st| st.text_color(theme.text))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |ws, _, window, cx| {
                    match (ahead, behind) {
                        (0, _) => ws.git_pull(&GitPull, window, cx),
                        (_, 0) => ws.git_push(&GitPush, window, cx),
                        _ => ws.git_sync(&GitSync, window, cx),
                    }
                }))
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .when(ahead > 0, |el| el.child(count(Icon::ArrowUp, ahead)))
                .when(behind > 0, |el| el.child(count(Icon::ArrowDown, behind))),
        )
    }

    /// Shown while the project isn't trusted; opens the trust dialog.
    /// `label`: with "Restricted Mode" beside its icon.
    pub(super) fn render_restricted_chip(&self, label: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        div()
            .id("restricted-chip")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            // A pill like the branch chip beside it: it's a button too.
            .h(px(CHIP_HEIGHT))
            .pl(px(10.))
            .pr(px(if label { 12. } else { 10. }))
            .rounded_full()
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(theme.warning)
            .hover_fill(Hsla::from(theme.warning).alpha(0.12), Hsla::from(theme.warning).alpha(0.2))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.open_trust_dialog(window, cx)))
            .tooltip(|_, cx| tooltip_view("Language servers, formatters and project tools are off until you trust this folder", cx))
            .child(icon_el(Icon::ShieldAlert, px(14.), theme.warning.into()))
            .when(label, |el| el.child("Restricted Mode"))
    }

    /// The checked-out branch, or what HEAD is when it isn't one.
    pub(super) fn branch_label(&self) -> String {
        let s = &self.git_status;
        s.branch.clone().unwrap_or_else(|| match &s.head_oid {
            Some(oid) => format!("{} (detached)", &oid[..7.min(oid.len())]),
            None => "No commits yet".into(),
        })
    }

    pub(super) fn render_window_controls(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        window_controls(window, cx)
    }

    pub(super) fn update_window_title(&mut self, window: &mut Window, cx: &App) {
        let mut title = match self.active_item(cx) {
            Some(item) => format!("{}{}", item.tab_title(cx), if item.is_dirty(cx) { " •" } else { "" }),
            None => "Kuana".into(),
        };
        if let Some(root) = self.root.as_deref() {
            title.push_str(&format!(" — {}", folder_name(root)));
        }
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }
}

/// The project, branch and Restricted Mode pills' height: a full pill that,
/// raised a little (`chip_top`), still keeps clear of the islands just below
/// under the pointer.
const CHIP_HEIGHT: f32 = 24.;

/// Room above the chips and the traffic lights in the tab row: they sit a
/// little above the tabs' names (the tabs hang from the islands), centered
/// between the window's top and the islands as near as a pill this tall allows
/// while leaving 5pt below it. Classic tabs fill their row: centered with them.
pub(super) fn chip_top(theme: &Theme) -> f32 {
    if theme.classic { 0. } else { 3. }
}

/// Between the chips above the sidebar.
const CHIP_GAP: f32 = 4.;
/// A chip's padding, icon and gaps around its name (see the `render_*_chip`s).
const PROJECT_CHROME: f32 = 3. + 18. + 8. + 10.;
const PROJECT_CHROME_PLAIN: f32 = 10. + 10.;
/// Just the monogram.
const PROJECT_MONOGRAM: f32 = 3. + 18. + 3.;
const BRANCH_CHROME: f32 = 10. + 14. + 6. + 12.;
/// A branch or Restricted Mode chip that's just its icon.
const ICON_CHIP: f32 = 10. + 14. + 10.;
const RESTRICTED_LABEL_CHROME: f32 = 10. + 14. + 6. + 12.;
/// The commits to push and pull, about.
const SYNC_COUNTS_WIDTH: f32 = 64.;
/// The title bar's lead as last drawn (`Workspace::title_ends_now`): where its
/// chips start, how wide they'd be there, and whether the sidebar showed.
#[derive(Clone, Copy, Default)]
pub(super) struct LeadLayout {
    base: f32,
    chips: f32,
    sidebar: bool,
}

/// The least of a name worth showing, about four characters and "…".
const MIN_NAME: f32 = 36.;

/// A name's measured width and its length.
#[derive(Clone, Copy)]
pub(super) struct Text {
    width: f32,
    chars: usize,
}

impl Text {
    /// How many of its characters fit `width`, the "…" among them.
    fn chars_in(self, width: f32) -> usize {
        if width >= self.width { self.chars } else { ((width / (self.width / self.chars as f32)).floor() as usize).max(1) }
    }
}

/// What the chips above the sidebar have to fit (see `fit_chips`).
pub(super) struct ChipNeeds {
    room: f32,
    project: Option<Text>,
    branch: Option<Text>,
    counts: Option<f32>,
    /// Restricted Mode's chip with its label, when the project isn't trusted.
    restricted: Option<f32>,
    avatar: bool,
}

/// How the chips show: names as characters (`None`, just the icon or
/// monogram), the project's monogram, the sync counts, and Restricted Mode
/// (`Some(label)`).
#[derive(Debug, PartialEq, Default)]
pub(super) struct ChipFit {
    project: Option<Option<usize>>,
    monogram: bool,
    branch: Option<Option<usize>>,
    counts: bool,
    restricted: Option<bool>,
}

/// Fits the chips to their room. All as they are when they fit; else the
/// sync counts go and Restricted Mode keeps just its icon, then the branch's
/// name shortens, then the branch is just its icon (the project's name stays
/// whole through all that), then the project's name shortens, then the
/// branch is gone, and last the project is just its monogram. The monogram stays throughout (`project_avatar`
/// turns it off), and Restricted Mode, a warning, while anything does.
pub(super) fn fit_chips(n: ChipNeeds) -> ChipFit {
    let gap = CHIP_GAP;
    let with_gap = |w: Option<f32>| w.map_or(0., |w| w + gap);
    let chrome = if n.avatar { PROJECT_CHROME } else { PROJECT_CHROME_PLAIN };
    let names = with_gap(n.project.map(|t| chrome + t.width)) + with_gap(n.branch.map(|t| BRANCH_CHROME + t.width));
    let natural = ChipFit {
        project: n.project.map(|t| Some(t.chars)),
        monogram: n.avatar,
        branch: n.branch.map(|t| Some(t.chars)),
        counts: false,
        restricted: None,
    };
    if names + with_gap(n.counts) + with_gap(n.restricted) <= n.room {
        return ChipFit { counts: n.counts.is_some(), restricted: n.restricted.map(|_| true), ..natural };
    }
    let restricted = n.restricted.map(|_| false);
    let natural = ChipFit { restricted, ..natural };
    let room = n.room - if restricted.is_some() { ICON_CHIP + gap } else { 0. };
    if names <= room {
        return natural;
    }
    let Some(p) = n.project else {
        // A repository's branch without a project folder doesn't happen; just the warning.
        return ChipFit { restricted, ..Default::default() };
    };
    let left = |others: f32| room - chrome - others;
    if let Some(b) = n.branch {
        // The branch gives way first, down to its icon, before the project's name.
        let for_branch = left(gap + BRANCH_CHROME) - p.width;
        if for_branch >= MIN_NAME.min(b.width) {
            return ChipFit { project: Some(Some(p.chars)), branch: Some(Some(b.chars_in(for_branch))), ..natural };
        }
        if left(gap + ICON_CHIP) >= MIN_NAME.min(p.width) {
            return ChipFit { project: Some(Some(p.chars_in(left(gap + ICON_CHIP)))), branch: Some(None), ..natural };
        }
    }
    if left(0.) >= MIN_NAME {
        return ChipFit { project: Some(Some(p.chars_in(left(0.)))), branch: None, ..natural };
    }
    let monogram = n.avatar && room >= PROJECT_MONOGRAM;
    ChipFit { project: monogram.then_some(None), monogram, branch: None, counts: false, restricted }
}

/// The monogram's letter: dark or light, whichever reads better on its tint
/// (light themes' tints are often dark). With gradients, the tint fades toward
/// the chrome, so it's judged partway along.
pub(super) fn monogram_letter(tint: Hsla, theme: &Theme) -> Hsla {
    let behind = if theme.gradients { transition::mix(theme.panel, tint, 0.8) } else { tint };
    let (dark, light) = (gpui::hsla(0., 0., 0., 1.), gpui::hsla(0., 0., 1., 1.));
    if crate::contrast::lc(dark, behind).abs() >= crate::contrast::lc(light, behind).abs() {
        dark.alpha(0.78)
    } else {
        light.alpha(0.92)
    }
}

/// A stable tint per project name for its monogram.
pub(super) fn monogram_color(name: &str, theme: &Theme) -> Hsla {
    let palette = [theme.accent, theme.success, theme.info, theme.warning, theme.syntax.function, theme.syntax.keyword, theme.git_added, theme.git_modified];
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    palette[hash as usize % palette.len()].into()
}

/// Windows' minimize, maximize and close buttons, for windows drawing their
/// own title bar (the workspace's, Preferences').
pub fn window_controls(window: &Window, cx: &App) -> impl IntoElement + use<> {
    let theme = Theme::get(cx);
    let font = crate::theme::caption_font(cx);
    let button = |id: &'static str, glyph: &'static str, area: WindowControlArea, hover: Hsla| {
        div()
            .id(id)
            .w(px(46.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(10.))
            .text_color(theme.text_muted)
            .font_family(font)
            .hover(move |s| s.bg(hover))
            .window_control_area(area)
            .child(glyph)
    };
    // Maximized, the middle one restores (and shows it).
    let max = if window.is_maximized() { "\u{E923}" } else { "\u{E922}" };
    div()
        .flex()
        .h_full()
        .child(button("min", "\u{E921}", WindowControlArea::Min, theme.hover.into()))
        .child(button("max", max, WindowControlArea::Max, theme.hover.into()))
        .child(button("close", "\u{E8BB}", WindowControlArea::Close, rgb(0xc42b1c).into()))
}

#[cfg(test)]
mod tests {
    use super::{ChipFit, ChipNeeds, Text, fit_chips, monogram_letter};
    use crate::theme::Theme;

    /// Dark on a light tint, light on a dark one.
    #[test]
    fn monogram_letters_read_on_their_tint() {
        let mut theme = Theme::blank_for_tests();
        theme.gradients = false;
        assert!(monogram_letter(gpui::rgba(0xf7d26eff).into(), &theme).l < 0.5);
        assert!(monogram_letter(gpui::rgba(0x4078f2ff).into(), &theme).l > 0.5);
        assert!(monogram_letter(gpui::rgba(0xa626a4ff).into(), &theme).l > 0.5);
    }

    /// The chips give way in order as their room narrows, never past it.
    #[test]
    fn chips_fit_their_room() {
        // "kuana-editor" and "ui/activity-bar", about 7.5pt a character.
        let needs = |room: f32| ChipNeeds {
            room,
            project: Some(Text { width: 90., chars: 12 }),
            branch: Some(Text { width: 112., chars: 15 }),
            counts: Some(64.),
            restricted: None,
            avatar: true,
        };
        let all = ChipFit { project: Some(Some(12)), monogram: true, branch: Some(Some(15)), counts: true, restricted: None };
        assert_eq!(fit_chips(needs(400.)), all);
        // No room for the counts.
        assert_eq!(fit_chips(needs(300.)), ChipFit { counts: false, ..all });
        // The branch shortens; the project's name and monogram stay whole.
        let fit = fit_chips(needs(220.));
        assert_eq!((fit.project, fit.monogram), (Some(Some(12)), true));
        assert!(matches!(fit.branch, Some(Some(n)) if n < 15));
        // Then the branch is its icon, the project still whole.
        let fit = fit_chips(needs(180.));
        assert_eq!((fit.project, fit.branch), (Some(Some(12)), Some(None)));
        // Then the project shortens beside it, then the branch is gone, then the project its monogram.
        let fit = fit_chips(needs(160.));
        assert!(matches!(fit.project, Some(Some(n)) if n < 12));
        assert_eq!(fit.branch, Some(None));
        assert_eq!(fit_chips(needs(120.)).branch, Some(None));
        assert_eq!(fit_chips(needs(80.)).branch, None);
        assert!(fit_chips(needs(80.)).monogram);
        assert_eq!(fit_chips(needs(40.)), ChipFit { project: Some(None), monogram: true, ..Default::default() });
        // Restricted Mode keeps its icon while the rest gives way.
        let fit = fit_chips(ChipNeeds { restricted: Some(140.), ..needs(220.) });
        assert_eq!(fit.restricted, Some(false));
        assert!(fit.project.is_some());
    }
}
