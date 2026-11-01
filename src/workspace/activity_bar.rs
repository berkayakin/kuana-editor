//! The activity bar: a column on the sidebar's side, as in VS Code. The
//! sidebar's views (Files, Commit) are vertical tabs at its top, the current
//! one shaped like the editor's current tab turned on its side and joined to
//! the sidebar's island; under them the bottom panels' toggles; at its foot
//! Search Everywhere, Extensions and Settings.

use super::*;
use gpui::svg;

/// A view tab's height.
const VIEW_TAB_HEIGHT: f32 = 40.0;
/// The bar's other buttons: a square hover target.
const ACTIVITY_BUTTON: f32 = 34.0;
/// Between the bar's buttons.
const ACTIVITY_SPACING: f32 = 4.0;
/// How far past a view's icon (20pt) its count may reach: the room to the
/// island less 6pt.
const BADGE_ROOM: f32 = (ACTIVITY_WIDTH - 20.) / 2. - 6.;

/// The line between the views and the panels' toggles, and its margin above and below.
const SEPARATOR: f32 = 1.0;
const SEPARATOR_MARGIN: f32 = 6.0;

/// One of the activity bar's buttons other than the views.
struct Tool {
    id: &'static str,
    icon: Icon,
    label: &'static str,
    /// Its panel or page shows.
    on: bool,
    /// A bottom panel's toggle, above the separator at the bar's foot; else
    /// one of the window's tools under it.
    panel: bool,
    /// Kept before those with a larger number when the bar is too short for
    /// them all (the rest go to its More menu).
    priority: u8,
    /// For its shortcut in the tooltip and the More menu.
    action: Box<dyn gpui::Action>,
    run: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
}

/// Which of `tools` fit in a bar `room` tall (under its top padding, over its
/// bottom one) besides the views, in their places' order: as many as fit by
/// priority, with a More button for the rest.
fn fit_tools(tools: &[Tool], room: f32) -> Vec<usize> {
    let button = ACTIVITY_BUTTON + ACTIVITY_SPACING;
    // The two views and the separator, each with the gap after it.
    let views = 2. * (VIEW_TAB_HEIGHT + ACTIVITY_SPACING) + SEPARATOR + 2. * SEPARATOR_MARGIN + ACTIVITY_SPACING;
    let fits = |n: usize| (room - views) / button >= n as f32;
    let mut order: Vec<usize> = (0..tools.len()).collect();
    if !fits(tools.len()) {
        order.sort_by_key(|&ix| tools[ix].priority);
        // One place goes to the More button.
        let room_for = (0..tools.len()).rev().find(|&n| fits(n + 1)).unwrap_or(0);
        order.truncate(room_for);
        order.sort_unstable();
    }
    order
}

/// How far a selected view's icon moves toward the island: from the buttons'
/// line (the bar's middle) to its tab's middle, the tab starting a gap
/// (`ACTIVITY_WIDTH - ACTIVITY_BUTTON`) / 2 off the window's edge.
const VIEW_TAB_SHIFT: f32 = (ACTIVITY_WIDTH - ACTIVITY_BUTTON) / 4.;

impl Workspace {
    /// The sidebar's side of the window, below the tab row: the activity bar
    /// and, while it shows, the sidebar's island beside it.
    pub(super) fn render_sidebar_block(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let right = self.sidebar_on_right(cx);
        // Classic: a separator line between the bar and the sidebar.
        let bar_room = ACTIVITY_WIDTH + if theme.classic && self.show_sidebar { 1. } else { 0. };
        let width = bar_room + if self.show_sidebar { f32::from(self.shown_sidebar_width()) } else { 0. };
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            // The tab row's part above it: the project and branch, beside the
            // title bar's lead or trail that sits over it; the rest moves the window.
            .child(tab_room(self.render_sidebar_room(width, window, cx), &theme))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .map(|el| if right { el.pr(px(bar_room)) } else { el.pl(px(bar_room)) })
                    .when(self.show_sidebar, |el| el.child(self.render_sidebar(cx)))
                    // After the island, so the current view's tab paints over its edge.
                    .child(
                        self.render_activity_bar(cx)
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .map(|el| if right { el.right_0() } else { el.left_0() }),
                    ),
            )
    }

    /// The tab row above the sidebar block, `width` wide: room for the title
    /// bar's lead (docked left) or trail (docked right) laid over it, the
    /// chips while the sidebar shows, and free space that moves the window.
    fn render_sidebar_room(&self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::get(cx);
        let right = self.sidebar_on_right(cx);
        let (lead_end, trail_start) = self.title_ends.get();
        // Docked left the block starts at the window's edge, docked right it ends there.
        let lead = if right { 0. } else { lead_end.max(0.) };
        let trail = if right { (f32::from(self.viewport.width) - trail_start).max(0.) } else { 0. };
        let room = width - lead - trail - TITLE_END_GAP;
        div()
            .flex()
            .items_center()
            .w_full()
            .pt(px(super::title_bar::chip_top(theme)))
            .child(window_drag_area("sidebar-room-lead").flex_shrink_0().w(px(lead)))
            .when(self.show_sidebar, |el| el.children(self.render_title_chips(room, window, cx)))
            .child(window_drag_area("sidebar-room").flex_1().min_w_0())
            .child(window_drag_area("sidebar-room-trail").flex_shrink_0().w(px(trail)))
    }

    /// Where the current view's tab joins the sidebar's island, so the
    /// island's edge light starts under it.
    pub(super) fn view_tab_joint(&self, cx: &App) -> crate::ui::SideJoint {
        let index = match self.sidebar_mode {
            SidebarMode::Files => 0.,
            SidebarMode::Commit => 1.,
        };
        let top = TAB_STRIP_INSET + index * (VIEW_TAB_HEIGHT + ACTIVITY_SPACING);
        crate::ui::SideJoint { right: self.sidebar_on_right(cx), below: top + VIEW_TAB_HEIGHT + TAB_FOOT }
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        // Its buttons are centered on it: as far from the window's edge as
        // from the island's (`ACTIVITY_WIDTH` = a button and two gaps).
        let sep = || div().flex_shrink_0().w(px(18.)).h(px(SEPARATOR)).my(px(SEPARATOR_MARGIN)).bg(theme.border);
        let top = if theme.classic { 0. } else { TAB_STRIP_INSET };
        // The bar's height: under the tab row, above the status bar.
        let height = f32::from(self.viewport.height) - (TAB_STRIP_HEIGHT - 1.) - STATUSBAR_HEIGHT - if theme.classic { 1. } else { 0. };
        let tools = self.activity_tools(cx);
        let shown = fit_tools(&tools, height - top - GAP);
        let hidden: Vec<usize> = (0..tools.len()).filter(|i| !shown.contains(i)).collect();
        let button = |ix: usize, cx: &mut Context<Self>| {
            let tool = &tools[ix];
            let run = tool.run;
            let (label, action) = (tool.label, tool.action.boxed_clone());
            self.activity_button(tool.id, tool.icon, tool.on, cx, move |ws, _, window, cx| run(ws, window, cx))
                .map(|el| match tool.id {
                    // Its shortcut is a double Shift, which no binding holds.
                    "search-everywhere" => el.tooltip(|_, cx| tooltip_view(format!("Search Everywhere ({})", double_shift_label()), cx)),
                    _ => el.tooltip(move |window, cx| status_tooltip(label, &*action, window, cx)),
                })
                .into_any_element()
        };
        let panels: Vec<AnyElement> = shown.iter().filter(|&&ix| tools[ix].panel).map(|&ix| button(ix, cx)).collect();
        let foot: Vec<AnyElement> = shown.iter().filter(|&&ix| !tools[ix].panel).map(|&ix| button(ix, cx)).collect();
        // The separator goes between the panels' toggles and the tools.
        let both = !panels.is_empty() && !foot.is_empty();
        let more = (!hidden.is_empty()).then(|| {
            let entries: Vec<(&'static str, Icon, Box<dyn gpui::Action>)> =
                hidden.iter().map(|&ix| (tools[ix].label, tools[ix].icon, tools[ix].action.boxed_clone())).collect();
            let lit = hidden.iter().any(|&ix| tools[ix].on);
            self.activity_button("activity-more", Icon::More, lit, cx, move |ws, event, window, cx| {
                let items = entries.iter().map(|(label, icon, _)| MenuEntry::item(*label, *icon)).collect();
                let actions = entries.iter().map(|(_, _, action)| Some(MenuAction::Run(action.boxed_clone()))).collect();
                ws.show_context_menu(items, actions, event.position(), None, window, cx);
            })
            // No tooltip: it would cover the menu it opens, under the pointer.
        });
        div()
            .id("activity-bar")
            .flex()
            .flex_col()
            .items_center()
            .w(px(ACTIVITY_WIDTH))
            // Modern: the first tab's foot lands on the island's straight edge.
            .pt(px(top))
            .pb(px(GAP))
            .gap(px(ACTIVITY_SPACING))
            // Not clipped: the current view's tab reaches 1px into the island,
            // over its border (`fit_tools` keeps the buttons within the bar).
            .when(theme.classic, |el| el.bg(theme.panel))
            .on_mouse_down(MouseButton::Right, cx.listener(|ws, e: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                ws.open_sidebar_tab_menu(e.position, window, cx);
            }))
            // Only the views are tabs, at the top; the buttons that open
            // panels and pages are grouped at the foot, so none reads as one.
            .child(self.render_view_tab(SidebarMode::Files, cx))
            .child(self.render_view_tab(SidebarMode::Commit, cx))
            .child(div().flex_1().min_h_0())
            .children(more)
            .children(panels)
            .when(both, |el| el.child(sep()))
            .children(foot)
    }

    /// The bar's buttons other than the views, in their places' order at its
    /// foot: the bottom panels' toggles (an open one lit; the less used ones
    /// only while there's something for them: results, a repository, a debug
    /// session), then Search Everywhere, Extensions and Settings (lit while its
    /// page shows).
    fn activity_tools(&self, cx: &App) -> Vec<Tool> {
        let tool = |id, icon, label, on, panel, priority, action: Box<dyn gpui::Action>, run| Tool { id, icon, label, on, panel, priority, action, run };
        let mut tools = vec![
            tool("toggle-terminal", Icon::Terminal, "Terminal", self.shows(BottomPanel::Terminal), true, 0, Box::new(ToggleTerminal), |ws, window, cx| {
                // Lit, it hides the panel even while the editor has the keyboard.
                ws.set_terminal_shown(!ws.shows(BottomPanel::Terminal), window, cx)
            }),
            tool("toggle-problems", Icon::PanelBottom, "Problems", self.shows(BottomPanel::Problems), true, 2, Box::new(ToggleProblems), |ws, window, cx| {
                ws.toggle_problems(&ToggleProblems, window, cx)
            }),
        ];
        if self.has_results {
            tools.push(tool("toggle-results", Icon::TextSearch, "Find", self.shows(BottomPanel::Results), true, 5, Box::new(ToggleResults), |ws, window, cx| {
                ws.toggle_results(&ToggleResults, window, cx)
            }));
        }
        if self.repo.is_some() {
            tools.push(tool("toggle-git-log", Icon::GitCommit, "Git Log", self.shows(BottomPanel::GitLog), true, 4, Box::new(ToggleGitLog), |ws, window, cx| {
                ws.toggle_git_log(&ToggleGitLog, window, cx)
            }));
        }
        if self.debug.is_some() {
            // While debugging, its panel matters most after the terminal.
            tools.push(tool("toggle-debug", Icon::Bug, "Debug", self.shows(BottomPanel::Debug), true, 1, Box::new(ToggleDebugPanel), |ws, window, cx| {
                ws.toggle_debug_panel(&ToggleDebugPanel, window, cx)
            }));
        }
        let page = self.preferences_shown(cx);
        tools.push(tool("search-everywhere", Icon::Search, "Search Everywhere", false, false, 3, Box::new(SearchEverywhere), |ws, window, cx| {
            ws.search_everywhere(&SearchEverywhere, window, cx)
        }));
        // The Extensions button stands for its own page.
        tools.push(tool("open-extensions", Icon::Puzzle, "Extensions", page == Some(true), false, 6, Box::new(OpenExtensions), |ws, window, cx| {
            ws.open_extensions(&OpenExtensions, window, cx)
        }));
        tools.push(tool("open-settings", Icon::Settings, "Settings", page == Some(false), false, 1, Box::new(OpenSettings), |ws, window, cx| {
            ws.open_settings(&OpenSettings, window, cx)
        }));
        tools
    }

    /// A sidebar view as a vertical tab. While the sidebar shows it, it's the
    /// island's color and joined to it (`view_tab_shape`); clicking it then
    /// hides the sidebar, like VS Code's and JetBrains' tool window buttons.
    fn render_view_tab(&self, mode: SidebarMode, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut theme = crate::ui::chrome_theme(Theme::get(cx));
        let on = self.show_sidebar && self.sidebar_mode == mode;
        // Lit with the tree's island while a drag targets the project folder.
        if on && mode == SidebarMode::Files && self.tree_root_drop {
            theme.background = crate::ui::drop_lift(&theme).1.into();
        }
        let right = self.sidebar_on_right(cx);
        let (id, icon) = match mode {
            SidebarMode::Files => ("activity-files", Icon::Folder),
            SidebarMode::Commit => ("activity-commit", Icon::GitBranch),
        };
        let changes = if mode == SidebarMode::Commit { self.git_status.files.len() } else { 0 };
        let (rest, hover): (Hsla, Hsla) = if on { (theme.accent.into(), theme.accent.into()) } else { (theme.text_dim.into(), theme.text.into()) };
        let tab = div()
            .id(id)
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .w_full()
            .h(px(VIEW_TAB_HEIGHT))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |ws, _, window, cx| match mode {
                SidebarMode::Files => ws.toggle_sidebar(&ToggleSidebar, window, cx),
                SidebarMode::Commit => ws.toggle_commit(&ToggleCommit, window, cx),
            }))
            .map(|el| match mode {
                SidebarMode::Files => el.tooltip(|window, cx| status_tooltip("Files", &ToggleSidebar, window, cx)),
                SidebarMode::Commit => {
                    // The count as it is now, also while the tooltip is shown.
                    let workspace = cx.entity();
                    el.tooltip(move |window, cx| {
                        let keys = binding_label(&ToggleCommit, window);
                        crate::ui::entity_tooltip_view(
                            &workspace,
                            move |ws, _| {
                                let label = match ws.git_status.files.len() {
                                    0 => "Commit".to_string(),
                                    changes => format!("Commit ({changes} changed)"),
                                };
                                with_keys(&label, keys.as_deref())
                            },
                            cx,
                        )
                    })
                }
            })
            .map(|el| if on { el.child(view_tab_shape(right, &theme)) } else { el });
        // Selected, the icon glides to the middle of its tab, which starts a gap
        // off the window's edge and runs into the island; elsewhere it's on the
        // buttons' line. The Classic tab spans the bar: nothing to move.
        let shift = if theme.classic { 0. } else if right { -VIEW_TAB_SHIFT } else { VIEW_TAB_SHIFT };
        let glyph = div()
            .relative()
            .size(px(20.))
            .flex_shrink_0()
            .child(svg().path(icon.path()).size(px(20.)).text_color(rest))
            // A resting tab's icon brightens on hover.
            .when(!on, |el| {
                el.child(
                    svg()
                        .path(icon.path())
                        .size(px(20.))
                        .absolute()
                        .top_0()
                        .left_0()
                        .text_color(hover)
                        .invisible()
                        .group_hover(id, |s| s.visible()),
                )
            })
            // Uncommitted changes, so they show from the Files view too.
            .when(changes > 0, |el| el.child(count_badge(changes, &theme)))
            .with_transition(SharedString::from(format!("{id}-shift")), on, transition::FOCUS, move |el, t| el.left(px(shift * t)));
        // The same elements selected or not, so the glide keeps its state. A
        // resting tab is a plain button: a rounded fill on hover.
        let fill: Hsla = if on { Hsla::from(theme.hover).alpha(0.) } else { theme.hover.into() };
        tab.group(id).child(
            div()
                .id(SharedString::from(format!("{id}-fill")))
                .size(px(ACTIVITY_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(9.))
                .hover_fill(Hsla::from(theme.hover).alpha(0.), fill)
                .child(glyph),
        )
    }

    /// An activity bar button: the icon in a square that fills on hover; `on`
    /// (an open panel or page) in the accent on a soft accent fill.
    fn activity_button(
        &self,
        id: &'static str,
        icon: Icon,
        on: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> crate::ui::Button {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let (rest, hover): (Hsla, Hsla) = if on { (theme.accent.into(), theme.accent.into()) } else { (theme.text_dim.into(), theme.text.into()) };
        let (fill, fill_hover): (Hsla, Hsla) = if on {
            (theme.accent_soft.into(), Hsla::from(theme.accent).alpha(0.24))
        } else {
            (Hsla::from(theme.hover).alpha(0.), theme.hover.into())
        };
        div()
            .id(id)
            .size(px(ACTIVITY_BUTTON))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(9.))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(on_click))
            // The fill and the icon ease together (an svg takes its own color,
            // so it's added here, from the progress).
            .with_hover_transition(SharedString::from(format!("{id}-hover")), transition::FOCUS, move |el, t| {
                el.bg(transition::mix(fill, fill_hover, t))
                    .child(svg().path(icon.path()).size(px(19.)).flex_shrink_0().text_color(transition::mix(rest, hover, t)))
            })
    }
}

/// The current view's tab: the island's color, rounded on the side away from
/// the island, outlined in the island's border down to concave feet that join
/// its edge, like the editor's current tab (`tab_shape`) turned on its side.
/// It starts a gap off the window's edge, like the buttons' fills, so its
/// middle is `VIEW_TAB_SHIFT` toward the island from theirs. Painted from one
/// set of pixel-snapped coordinates, as the editor's is. Classic: a rectangle
/// that covers the separator, opening onto the sidebar.
fn view_tab_shape(right: bool, theme: &Theme) -> AnyElement {
    let island: Hsla = theme.background.into();
    if theme.classic {
        return div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(if right { -1. } else { 0. }))
            .right(px(if right { 0. } else { -1. }))
            .bg(island)
            .into_any_element();
    }
    let edge = crate::ui::island_edge(theme);
    let outer = (ACTIVITY_WIDTH - ACTIVITY_BUTTON) / 2.;
    gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let s = window.scale_factor();
            let snap = |v: Pixels| px((f32::from(v) * s).round() / s);
            let (r, radius, one) = (px(TAB_FOOT), px(TAB_RADIUS), px(1.));
            let (left, right_x) = (snap(bounds.left()), snap(bounds.right()));
            let (top, bottom) = (snap(bounds.top() + r), snap(bounds.bottom() - r));
            let corners = if right {
                gpui::Corners { top_right: radius, bottom_right: radius, ..Default::default() }
            } else {
                gpui::Corners { top_left: radius, bottom_left: radius, ..Default::default() }
            };
            window.paint_quad(gpui::fill(Bounds::from_corners(point(left, top), point(right_x, bottom)), island).corner_radii(corners));
            // The outline stops a foot short of the island; the feet's curves continue it.
            let (from, to) = if right { (left + r, right_x) } else { (left, right_x - r) };
            window.paint_quad(gpui::quad(
                Bounds::from_corners(point(from, top), point(to, bottom)),
                corners,
                // Not transparent black (see `tab_shape`).
                edge.alpha(0.),
                gpui::Edges { top: one, bottom: one, left: if right { px(0.) } else { one }, right: if right { one } else { px(0.) } },
                edge,
                Default::default(),
            ));
            // The squares along the island's edge, above and below the tab,
            // each curving away from the tab and the island.
            let square_x = if right { left } else { right_x - r };
            let center_x = if right { left + r } else { right_x - r };
            let above = top - r + one;
            let below = bottom - one;
            paint_concave(window, Bounds { origin: point(square_x, above), size: size(r, r) }, point(center_x, above), island, Some(edge));
            paint_concave(window, Bounds { origin: point(square_x, below), size: size(r, r) }, point(center_x, below + r), island, Some(edge));
        },
    )
    .absolute()
    .top(px(-TAB_FOOT))
    .bottom(px(-TAB_FOOT))
    // 1px into the island, over its border, which the tab opens.
    .left(px(if right { -1. } else { outer }))
    .right(px(if right { outer } else { -1. }))
    .into_any_element()
}

/// Dark or light text, whichever reads better on `fill`.
fn badge_text(fill: Hsla) -> Hsla {
    let (dark, light) = (gpui::hsla(0., 0., 0., 0.85), gpui::hsla(0., 0., 1., 1.));
    if crate::contrast::lc(dark, fill).abs() >= crate::contrast::lc(light, fill).abs() { dark } else { light }
}

/// A small count over an icon's corner (the Commit view's changes). No ring
/// around it: the chrome behind is a gradient and the button's fill changes
/// on hover, so a ring in any one color would show as a halo. It hangs below
/// the icon's lower right corner, covering little of it; a count too long
/// for the room beside the icon starts further in, so it keeps `BADGE_ROOM`
/// past the icon's edge, 6pt from the island.
fn count_badge(count: usize, theme: &Theme) -> impl IntoElement + use<> {
    let fill: Hsla = theme.accent.into();
    let label = if count > 99 { "99+".to_string() } else { count.to_string() };
    // Padding plus a 9pt bold digit's width (as drawn) each.
    let width = 6. + 7. * label.chars().count() as f32;
    div()
        .absolute()
        // Hanging below the icon's corner: it covers little of it.
        .bottom(px(-9.))
        .left(px(13_f32.min(20. + BADGE_ROOM - width)))
        .h(px(13.))
        .min_w(px(13.))
        .px(px(3.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.5))
        .bg(fill)
        .text_size(px(9.))
        .line_height(px(13.))
        .font_weight(FontWeight::BOLD)
        .text_color(badge_text(fill))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(priorities: &[u8]) -> Vec<Tool> {
        priorities
            .iter()
            .map(|&priority| Tool {
                id: "tool",
                icon: Icon::Terminal,
                label: "Tool",
                on: false,
                panel: true,
                priority,
                action: Box::new(ToggleTerminal),
                run: |_, _, _| {},
            })
            .collect()
    }

    /// A tall bar shows every tool; a short one the most important, in their
    /// places' order, with a place left for the More button.
    #[test]
    fn tools_fit_the_bar_by_priority() {
        let all = tools(&[0, 2, 4, 3, 6, 1]);
        assert_eq!(fit_tools(&all, 2000.), vec![0, 1, 2, 3, 4, 5]);
        let views = 2. * (VIEW_TAB_HEIGHT + ACTIVITY_SPACING) + SEPARATOR + 2. * SEPARATOR_MARGIN + ACTIVITY_SPACING;
        let button = ACTIVITY_BUTTON + ACTIVITY_SPACING;
        // Room for four buttons: three tools and More.
        assert_eq!(fit_tools(&all, views + 4. * button), vec![0, 1, 5]);
        // Exactly all six: no More.
        assert_eq!(fit_tools(&all, views + 6. * button).len(), 6);
        // Not even one beside More.
        assert!(fit_tools(&all, views).is_empty());
    }
}
