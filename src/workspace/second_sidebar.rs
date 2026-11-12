//! The second sidebar: an island on the editor's other side for views moved
//! out of the sidebar (a view's tab menu: Move to Right / Left Side). A view
//! is in one place at a time; its activity bar tab stays where it was and
//! shows or hides it there. It has no tabs, whatever it holds: the row above
//! it is the window's (to drag it, and for the window's buttons on Windows
//! and Linux). Its island starts with a header: the view's name (with other
//! views here, a list of them to switch to), Move Back and Close.

use super::*;

/// The header's row, as tall as the Commit view's toolbar.
const HEADER_HEIGHT: f32 = 30.;

/// The moved views `state.json` remembers, without ones that are gone or
/// listed twice.
pub(super) fn restore_moved(saved: &[String]) -> Vec<SidebarMode> {
    let mut moved: Vec<SidebarMode> = Vec::new();
    for mode in saved.iter().filter_map(|name| SidebarMode::from_saved(name)) {
        if !moved.contains(&mode) {
            moved.push(mode);
        }
    }
    // The sidebar keeps a view of its own (`can_move`).
    if SidebarMode::ALL.iter().all(|mode| moved.contains(mode)) {
        moved.retain(|mode| *mode != SidebarMode::Files);
    }
    moved
}

/// A right-click on a view's tab or name: its menu.
fn view_menu(mode: SidebarMode, cx: &mut Context<Workspace>) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |ws, e: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        ws.open_view_menu(mode, e.position, window, cx);
    })
}

impl Workspace {
    /// Every view there is now, in the tabs' order.
    fn all_views(&self) -> Vec<SidebarMode> {
        SidebarMode::ALL.into_iter().chain(self.extension_modes().iter().copied()).collect()
    }

    pub(super) fn in_second(&self, mode: SidebarMode) -> bool {
        self.moved_views.contains(&mode)
    }

    /// The sidebar's own views.
    pub(super) fn sidebar_views(&self) -> Vec<SidebarMode> {
        self.all_views().into_iter().filter(|mode| !self.in_second(*mode)).collect()
    }

    /// The second sidebar's views, in the order they were moved.
    pub(super) fn second_views(&self) -> Vec<SidebarMode> {
        let all = self.all_views();
        self.moved_views.iter().copied().filter(|mode| all.contains(mode)).collect()
    }

    /// The sidebar's first view, for when its current one leaves.
    pub(super) fn first_sidebar_view(&self) -> SidebarMode {
        self.sidebar_views().first().copied().unwrap_or(SidebarMode::Files)
    }

    /// The view the second sidebar is open on.
    pub(super) fn second_open(&self) -> Option<SidebarMode> {
        self.second_view.filter(|mode| self.second_views().contains(mode))
    }

    /// Whether `mode` shows, in whichever sidebar it's in.
    pub(super) fn view_shown(&self, mode: SidebarMode) -> bool {
        if self.in_second(mode) { self.second_open() == Some(mode) } else { self.show_sidebar && self.sidebar_mode == mode }
    }

    /// Shows `mode` where it lives, or hides it (the sidebar itself, or the
    /// second sidebar when it's the view open there).
    pub(super) fn set_view_shown(&mut self, mode: SidebarMode, shown: bool) {
        if self.in_second(mode) {
            if shown {
                self.second_view = Some(mode);
            } else if self.second_view == Some(mode) {
                self.second_view = None;
            }
        } else if shown {
            self.show_sidebar = true;
            self.sidebar_mode = mode;
        } else if self.sidebar_mode == mode {
            self.show_sidebar = false;
        }
    }

    /// What the sidebar leaves the second one while it's open: its minimum
    /// width and the gap before it.
    pub(super) fn second_reserve(&self) -> f32 {
        if self.second_open().is_some() { SIDEBAR_MIN_WIDTH + GAP } else { 0. }
    }

    /// The width the second sidebar may take: what the sidebar and a pane's
    /// width leave of the window.
    pub(super) fn second_room(&self) -> Pixels {
        let sidebar = if self.show_sidebar { self.shown_sidebar_width() + px(GAP) } else { px(0.) };
        self.viewport.width - px(self.activity_width + 2. * GAP + MIN_PANE_WIDTH) - sidebar
    }

    /// Its width as shown; none in a window too narrow for it (it comes back
    /// as the window grows).
    pub(super) fn shown_second_width(&self) -> Option<Pixels> {
        let room = self.second_room();
        (room >= px(SIDEBAR_MIN_WIDTH)).then(|| self.second_width.min(room))
    }

    /// The view and width to draw it with, while it's open and fits.
    pub(super) fn second_shown(&self) -> Option<(SidebarMode, Pixels)> {
        self.second_open().zip(self.shown_second_width())
    }

    /// The sidebar keeps at least one of the built-in views, so it's never
    /// left with nothing (an extension's view may come and go).
    fn can_move(&self, mode: SidebarMode) -> bool {
        !self.in_second(mode) && SidebarMode::ALL.iter().any(|other| *other != mode && !self.in_second(*other))
    }

    /// Moves a view to the second sidebar and shows it there.
    fn move_view(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_move(mode) {
            return;
        }
        if let SidebarMode::Extension(slot) = mode
            && self.extension_view(slot, window, cx).is_none()
        {
            return;
        }
        self.moved_views.push(mode);
        self.second_view = Some(mode);
        if self.sidebar_mode == mode {
            self.sidebar_mode = self.first_sidebar_view();
        }
        self.views_moved(window, cx);
    }

    /// Brings a view back to the sidebar, showing it there; the second
    /// sidebar goes on to its next view, or closes with its last.
    fn move_view_back(&mut self, mode: SidebarMode, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_second(mode) {
            return;
        }
        let was_open = self.second_open() == Some(mode);
        self.moved_views.retain(|other| *other != mode);
        if was_open {
            self.second_view = self.second_views().first().copied();
        }
        self.show_sidebar = true;
        self.sidebar_mode = mode;
        self.views_moved(window, cx);
    }

    fn views_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_mode == SidebarMode::Commit || self.second_open() == Some(SidebarMode::Commit) {
            self.refresh_commit_view(cx);
        }
        self.release_hidden_focus(window, cx);
        self.save_state(cx);
        cx.notify();
    }

    fn close_second_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.second_view = None;
        self.release_hidden_focus(window, cx);
        self.save_state(cx);
        cx.notify();
    }

    /// Toggle Second Sidebar: closes it, or opens it on its first view.
    pub(super) fn toggle_second_sidebar(&mut self, _: &ToggleSecondSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if self.second_open().is_some() {
            self.close_second_sidebar(window, cx);
        } else if let Some(mode) = self.second_views().first().copied() {
            self.select_sidebar_view(mode, window, cx);
        } else {
            let side = if self.sidebar_on_right(cx) { "Left" } else { "Right" };
            self.toast(ToastKind::Info, "No views in the second sidebar", Some(format!("Right-click a view's tab and choose Move to {side} Side.")), cx);
        }
    }

    /// A view tab's right-click menu: the view to the other side or back,
    /// then the whole sidebar's position.
    pub(super) fn open_view_menu(&mut self, mode: SidebarMode, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let right = self.sidebar_on_right(cx);
        // The second sidebar's side, and the sidebar's.
        let (there, there_icon) = if right { ("Left", Icon::PanelLeft) } else { ("Right", Icon::PanelRight) };
        let (here, here_icon) = if right { ("Right", Icon::PanelRight) } else { ("Left", Icon::PanelLeft) };
        let name = mode.label();
        let mut entries = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let call = |run: fn(&mut Workspace, SidebarMode, &mut Window, &mut Context<Workspace>)| {
            Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| run(ws, mode, window, cx))))
        };
        if self.in_second(mode) {
            entries.push(MenuEntry::item(format!("Move {name} Back to {here} Side"), here_icon));
            actions.push(call(Self::move_view_back));
            if self.second_open() == Some(mode) {
                entries.push(MenuEntry::item("Close", Icon::X));
                actions.push(call(|ws, _, window, cx| ws.close_second_sidebar(window, cx)));
            }
        } else if self.can_move(mode) {
            entries.push(MenuEntry::item(format!("Move {name} to {there} Side"), there_icon));
            actions.push(call(Self::move_view));
        }
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
            actions.push(None);
        }
        entries.push(MenuEntry::item(format!("Move Sidebar to {there}"), there_icon));
        actions.push(Some(MenuAction::Run(Box::new(ToggleSidebarPosition))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The views here, to switch between: the header's name opens it.
    fn open_second_views_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let (mut entries, mut actions) = (Vec::new(), Vec::new());
        for view in self.second_views() {
            let icon = match view {
                SidebarMode::Files => Icon::Folder,
                SidebarMode::Commit => Icon::GitBranch,
                SidebarMode::Database => Icon::Database,
                SidebarMode::Extension(_) => Icon::Puzzle,
            };
            entries.push(MenuEntry::item(view.label(), icon));
            actions.push(Some(MenuAction::Call(std::rc::Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                ws.select_sidebar_view(view, window, cx)
            }))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The second sidebar: the tab row's room, free to move the window, over
    /// an island with its header (the view's name, Move Back, Close) and the view.
    pub(super) fn render_second_sidebar(&self, mode: SidebarMode, width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let views = self.second_views();
        let lone = views.len() == 1;
        let commit_toolbar = mode == SidebarMode::Commit && self.repo.is_some();
        let small_button = |id: &'static str, icon: Icon, tip: SharedString| {
            div()
                .id(id)
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .child(icon_el(icon, px(14.), theme.text_muted.into()))
        };
        let back_side = if self.sidebar_on_right(cx) { ("Right", Icon::PanelRight) } else { ("Left", Icon::PanelLeft) };
        let back = format!("Move {} Back to {} Side", mode.label(), back_side.0);
        let row = || div().flex().items_center().gap_0p5().h(px(HEADER_HEIGHT)).px_1().flex_shrink_0().border_b_1().border_color(theme.border_subtle);
        let buttons = [
            small_button("second-back", back_side.1, back.into()).on_click(cx.listener(move |ws, _, window, cx| ws.move_view_back(mode, window, cx))),
            small_button("second-close", Icon::X, "Close".into()).on_click(cx.listener(|ws, _, window, cx| ws.close_second_sidebar(window, cx))),
        ];
        // What names the view: alone, the Commit view's own toolbar (its
        // branch) stands for it; else its name, which with other views here
        // opens their list (they have no tabs: the row above is the window's).
        let toolbar_leads = lone && commit_toolbar;
        let lead: Vec<AnyElement> = if toolbar_leads {
            self.render_git_toolbar(cx).into_iter().collect()
        } else {
            let name = div()
                .id("second-title")
                .flex()
                .flex_shrink()
                .min_w_0()
                .items_center()
                .gap_1p5()
                .h(px(24.))
                .px_1p5()
                .rounded(px(5.))
                .text_size(px(12.5))
                .on_mouse_down(MouseButton::Right, view_menu(mode, cx))
                .child(gpui::svg().path(mode.icon_path()).size(px(14.)).flex_shrink_0().text_color(theme.text_dim))
                .child(div().truncate().text_color(theme.text).child(mode.label()))
                .when(!lone, |el| {
                    el.cursor_pointer()
                        .child(icon_el(Icon::ChevronDown, px(12.), theme.text_dim.into()))
                        .on_click(cx.listener(|ws, e: &ClickEvent, window, cx| ws.open_second_views_menu(e.position(), window, cx)))
                        // Not over the list it opened, under the pointer.
                        .when(self.context_menu.is_none(), |el| el.tooltip(|_, cx| tooltip_view("Views on This Side", cx)))
                })
                // Only a name that opens the list answers the pointer.
                .hover_fill(Hsla::from(theme.hover).alpha(0.), Hsla::from(theme.hover).alpha(if lone { 0. } else { theme.hover.a }))
                .into_any_element();
            vec![name, div().flex_1().into_any_element()]
        };
        // Beside the view's name, the Commit view's toolbar is a row of its own.
        let toolbar = (commit_toolbar && !toolbar_leads).then(|| row().mb_1().children(self.render_git_toolbar(cx)));
        let header = row().when(toolbar_leads, |el| el.mb_1()).children(lead).children(buttons);
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(width)
            .h_full()
            .child(tab_room(window_drag_area("second-sidebar-room").w_full(), &theme))
            .child(
                crate::ui::area(&theme)
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(header)
                    .children(toolbar)
                    .child(self.render_view_body(mode, cx))
                    // Deferred so the grab area sits above the editor it overlaps.
                    .child(gpui::deferred(self.resize_handle(Edge::SecondSidebar, cx))),
            )
            .into_any_element()
    }
}
