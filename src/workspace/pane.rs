//! A pane: tabs with their own strip above an island of the editor area. It
//! keeps its tabs (the current one, pins, how each tab looks) and draws them;
//! the workspace arranges panes, opens and closes tabs in them and answers
//! what a pane reports (`PaneEvent`), like the terminal panel. What a pane
//! shows of the workspace around it (the keyboard, the find bar) it's told
//! through `set_chrome`.
//!
//! The workspace draws each pane as a cached view: typing in one pane redraws
//! that pane, not the other panes' strips and breadcrumbs.

use super::*;

/// What a pane asks of the workspace.
pub(super) enum PaneEvent {
    /// A tab was clicked: it becomes the current tab, in the current pane.
    Activate(usize),
    /// A click in the pane outside its strip: it becomes the current pane.
    Focus,
    /// A tab's close button or a middle click (a pinned tab's pin unpins it).
    Tab(TabAction, PaneItem),
    /// A tab was right-clicked.
    Menu { item: PaneItem, position: gpui::Point<Pixels> },
    /// The tabs that don't fit, to pick from.
    Overflow { items: Vec<PaneItem>, position: gpui::Point<Pixels> },
    /// A tab was dropped here: before the tab at `before`, else after the others.
    Drop { item: PaneItem, before: Option<usize> },
    /// Files from Finder were dropped on the pane.
    OpenPaths(Vec<PathBuf>),
    /// A file from the tree was dropped on the pane.
    OpenPath(PathBuf),
}

impl gpui::EventEmitter<PaneEvent> for Pane {}

/// What a pane shows of the workspace around it, set by `Workspace::sync_panes`.
#[derive(Clone, PartialEq, Default)]
pub(super) struct PaneChrome {
    /// The current pane: the keyboard is in it, or was last.
    pub(super) active: bool,
    /// Outlined as the split that has the keyboard (the current one of several).
    pub(super) outlined: bool,
    /// Its strip runs along the window's top, so the strip's free space moves the window.
    pub(super) at_top: bool,
    /// The find bar, shown above a text tab: Find is open and this is the current pane.
    pub(super) find_bar: Option<Entity<FindBar>>,
    /// The Git diff covers the editor area with a tab of its own: the strip hides,
    /// so it doesn't seem to control the diff.
    pub(super) covered: bool,
    /// The project folder, where breadcrumbs start.
    pub(super) root: Option<PathBuf>,
}

pub(super) struct Pane {
    tabs: Vec<PaneItem>,
    active: usize,
    /// Pinned tabs stay first and survive Close Others / Close All.
    pinned: HashSet<EntityId>,
    /// What each tab shows, kept as its item changes: drawing the strip reads
    /// no item, so one changing out of sight (diagnostics arriving) redraws
    /// nothing unless its tab looks different.
    looks: HashMap<EntityId, TabLook>,
    /// Each tab's observation, which keeps its look.
    observers: HashMap<EntityId, Subscription>,
    chrome: PaneChrome,
    /// The pane's bounds last frame, which its tabs are fitted to.
    bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// A file from Finder or the tree is over the pane: its island lights up,
    /// and its current tab, which is part of it, too.
    file_drop: bool,
}

impl Pane {
    pub(super) fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            pinned: HashSet::new(),
            looks: HashMap::new(),
            observers: HashMap::new(),
            chrome: PaneChrome::default(),
            bounds: Default::default(),
            file_drop: false,
        }
    }

    pub(super) fn tabs(&self) -> &[PaneItem] {
        &self.tabs
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The index of the current tab (0 without tabs).
    pub(super) fn active_ix(&self) -> usize {
        self.active
    }

    pub(super) fn active_item(&self) -> Option<&PaneItem> {
        self.tabs.get(self.active)
    }

    /// Where a tab (by its view's id) is.
    pub(super) fn position(&self, id: EntityId) -> Option<usize> {
        self.tabs.iter().position(|t| t.entity_id() == id)
    }

    pub(super) fn is_pinned(&self, id: EntityId) -> bool {
        self.pinned.contains(&id)
    }

    pub(super) fn has_file_drop(&self) -> bool {
        self.file_drop
    }

    /// Where a new tab goes: after the current one, but never among pinned ones.
    pub(super) fn next_slot(&self) -> usize {
        let pinned = self.tabs.iter().filter(|t| self.pinned.contains(&t.entity_id())).count();
        if self.tabs.is_empty() { 0 } else { (self.active + 1).max(pinned).min(self.tabs.len()) }
    }

    /// Adds a tab at `ix` (clamped; pinned tabs stay first) without making it
    /// current, and keeps its look from now on; returns where it went.
    pub(super) fn insert(&mut self, item: PaneItem, ix: usize, pinned: bool, cx: &mut Context<Self>) -> usize {
        let id = item.entity_id();
        self.looks.insert(id, item.look(cx));
        let observer = match &item {
            PaneItem::Editor(editor) => cx.observe(editor, |pane, editor, cx| pane.look_changed(&PaneItem::Editor(editor), cx)),
            PaneItem::Image(image) => cx.observe(image, |pane, image, cx| pane.look_changed(&PaneItem::Image(image), cx)),
            PaneItem::Preview(preview) => cx.observe(preview, |pane, preview, cx| pane.look_changed(&PaneItem::Preview(preview), cx)),
        };
        self.observers.insert(id, observer);
        let current = self.tabs.get(self.active).cloned();
        self.tabs.insert(ix.min(self.tabs.len()), item);
        if pinned {
            self.pinned.insert(id);
        }
        // The current tab stays current.
        if let Some(current) = current {
            self.active = self.tabs.iter().position(|t| *t == current).unwrap_or(0);
        }
        self.keep_pinned_first();
        cx.notify();
        self.position(id).unwrap_or(0)
    }

    /// Takes a tab out; returns whether it was pinned. When it was the
    /// current tab, the one after it takes over (the one before, if it was
    /// the last).
    pub(super) fn remove(&mut self, id: EntityId, cx: &mut Context<Self>) -> Option<bool> {
        let ix = self.position(id)?;
        self.tabs.remove(ix);
        if ix < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.looks.remove(&id);
        self.observers.remove(&id);
        cx.notify();
        Some(self.pinned.remove(&id))
    }

    pub(super) fn set_active(&mut self, ix: usize, cx: &mut Context<Self>) {
        let ix = ix.min(self.tabs.len().saturating_sub(1));
        if ix != self.active {
            self.active = ix;
            cx.notify();
        }
    }

    pub(super) fn toggle_pin(&mut self, id: EntityId, cx: &mut Context<Self>) {
        if !self.pinned.remove(&id) {
            self.pinned.insert(id);
        }
        self.keep_pinned_first();
        cx.notify();
    }

    /// What the pane shows of the workspace; redrawn only when it changed.
    pub(super) fn set_chrome(&mut self, chrome: PaneChrome, cx: &mut Context<Self>) {
        if self.chrome != chrome {
            self.chrome = chrome;
            cx.notify();
        }
    }

    pub(super) fn set_file_drop(&mut self, over: bool, cx: &mut Context<Self>) {
        if self.file_drop != over {
            self.file_drop = over;
            cx.notify();
        }
    }

    /// Pinned tabs first, in their order; the current tab stays current.
    fn keep_pinned_first(&mut self) {
        let current = self.tabs.get(self.active).cloned();
        let pinned = &self.pinned;
        self.tabs.sort_by_key(|t| !pinned.contains(&t.entity_id()));
        if let Some(current) = current {
            self.active = self.tabs.iter().position(|t| *t == current).unwrap_or(0);
        }
    }

    /// A tab's item changed: the pane redraws only if its tab looks different.
    fn look_changed(&mut self, item: &PaneItem, cx: &mut Context<Self>) {
        let look = item.look(cx);
        if self.looks.get(&item.entity_id()) != Some(&look) {
            self.looks.insert(item.entity_id(), look);
            cx.notify();
        }
    }

    /// The tabs, like a browser's: on the chrome above the island, the
    /// current one filled like the island and joined to it. `edge` is the
    /// island's border color, which the current tab continues.
    fn render_strip(&self, edge: Hsla, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = cx.entity_id();
        let mut theme = Theme::get(cx).clone();
        if self.file_drop && cx.has_active_drag() {
            theme.background = crate::ui::drop_lift(&theme).1.into();
        }
        let accent: Hsla = theme.accent.into();
        let looks: Vec<TabLook> =
            self.tabs.iter().map(|t| self.looks.get(&t.entity_id()).cloned().unwrap_or_else(|| t.look(cx))).collect();
        let names: Vec<String> = looks.iter().map(|l| l.title.clone()).collect();
        let name_widths = measure_tab_names(&names, window, &theme);
        let naturals: Vec<f32> = looks
            .iter()
            .zip(&name_widths)
            .map(|(look, (w, _))| (TAB_CHROME + w + if look.errors { TAB_ERROR_DOT } else { 0. }).ceil())
            .collect();
        // The room for tabs: the pane less the strip's insets (unknown before the first frame).
        let width = f32::from(self.bounds.get().size.width);
        let available = if width > 0. { width - 2. * TAB_STRIP_INSET } else { f32::INFINITY };
        let slots = fit_tabs(&naturals, self.active, available);
        let visible: Vec<usize> = (0..slots.len()).filter(|&i| slots[i] != TabSlot::Hidden).collect();
        let hidden: Vec<PaneItem> = (0..slots.len()).filter(|&i| slots[i] == TabSlot::Hidden).map(|i| self.tabs[i].clone()).collect();
        let focused = self.chrome.active;
        div()
            .id("tabs")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .flex_row()
            .items_end()
            .pt(px(TAB_STRIP_TOP))
            // Clear of the island's rounded corners on both sides, so the first
            // and last tabs' feet sit on its straight top edge.
            .pl(px(TAB_STRIP_INSET))
            .pr(px(TAB_STRIP_INSET))
            // No scrolling, which would cut the current tab's shape at the edge:
            // like a browser, tabs shrink to fit, down to their icon.
            .overflow_hidden()
            // Dropping on the strip's empty part moves the tab to the end.
            .on_drop(cx.listener(|_, dragged: &DraggedTab, _, cx| {
                cx.emit(PaneEvent::Drop { item: dragged.item.clone(), before: None });
            }))
            .children(visible.iter().enumerate().map(|(pos, &ix)| {
                let item = &self.tabs[ix];
                let slot = slots[ix];
                let look = &looks[ix];
                let active = ix == self.active;
                let next = visible.get(pos + 1).copied();
                // Separators only between two tabs that sit on the chrome.
                let separator = !active && next.is_some_and(|n| n != self.active);
                let after_active = pos > 0 && visible[pos - 1] == self.active;
                let icon_only = matches!(slot, TabSlot::Icon(_));
                let compact = matches!(slot, TabSlot::Compact(_));
                let pinned = self.pinned.contains(&item.entity_id());
                let (dirty, errors) = (look.dirty, look.errors);
                let icon = match &look.icon {
                    TabIcon::File(path) => file_icon(path, px(15.), &theme).into_any_element(),
                    TabIcon::Ui(icon) => icon_el(*icon, px(14.), theme.accent.into()).into_any_element(),
                };
                let name = names[ix].clone();
                let shown_name = tab_label(&name, slot, naturals[ix], name_widths[ix]);
                let name_color: Hsla = if active && focused { theme.text.into() } else { theme.text_muted.into() };
                // Group names are looked up window-wide: the pane's id keeps them apart.
                let group = SharedString::from(format!("tab-{id}-{ix}"));
                let (for_close, for_menu, for_drag, for_unpin) = (item.clone(), item.clone(), item.clone(), item.clone());
                div()
                    .id(SharedString::from(format!("tab-{ix}")))
                    .group(group.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h_full()
                    .flex_shrink_0()
                    .map(|el| tab_slot_width(el, slot, naturals[ix]))
                    .cursor_pointer()
                    .map(|el| tab_shape(el, active, after_active, separator, group.clone(), edge, &theme))
                    // The full path, as Zed shows it; an untitled tab's name
                    // only when the tab doesn't show it whole.
                    .when_some(
                        look.path.as_deref().map(crate::clone_dialog::display_path).or_else(|| (icon_only || compact).then(|| name.clone())),
                        |el, tip| el.tooltip(move |_, cx| tooltip_view(tip.clone(), cx)),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::Activate(ix))))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::Tab(TabAction::Close, for_close.clone()))),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            cx.emit(PaneEvent::Menu { item: for_menu.clone(), position: event.position });
                        }),
                    )
                    .on_drag(DraggedTab { item: for_drag, name: name.clone().into() }, |tab, _, _, cx| {
                        cx.new(|_| DragLabel(tab.name.clone()))
                    })
                    // A drop lands before this tab; the accent edge shows where.
                    .drag_over::<DraggedTab>(move |style, _, _, _| style.border_l_2().border_color(accent))
                    .on_drop(cx.listener(move |_, dragged: &DraggedTab, _, cx| {
                        cx.stop_propagation();
                        cx.emit(PaneEvent::Drop { item: dragged.item.clone(), before: Some(ix) });
                    }))
                    .child(icon)
                    .when(!icon_only, |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(name_color)
                                .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                                .when(!active, |el| el.group_hover(group.clone(), |s| s.text_color(theme.text)))
                                .child(shown_name),
                        )
                    })
                    // Files with errors get a small dot rather than a recolored name.
                    .when(errors && !icon_only && !compact, |el| {
                        el.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(theme.error))
                    })
                    .when(!icon_only && !compact, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("close-{ix}")))
                                .flex_shrink_0()
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.))
                                .hover_bg(theme.active)
                                .on_click(cx.listener(move |pane, _, _, cx| {
                                    cx.stop_propagation();
                                    // A pinned tab's pin unpins it; otherwise this closes.
                                    let action =
                                        if pane.pinned.contains(&for_unpin.entity_id()) { TabAction::TogglePin } else { TabAction::Close };
                                    cx.emit(PaneEvent::Tab(action, for_unpin.clone()));
                                }))
                                .map(|el| {
                                    if pinned {
                                        el.child(icon_el(Icon::Pin, px(12.), theme.text_muted.into()))
                                    } else if dirty {
                                        // A dot that turns into a close button on hover.
                                        el.child(div().size(px(7.)).rounded_full().bg(theme.accent).group_hover(group.clone(), |s| s.invisible()))
                                            .child(
                                                div()
                                                    .absolute()
                                                    .invisible()
                                                    .group_hover(group.clone(), |s| s.visible())
                                                    .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                            )
                                    } else {
                                        el.child(
                                            div()
                                                .when(!active, |d| d.invisible())
                                                .group_hover(group.clone(), |s| s.visible())
                                                .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                        )
                                    }
                                }),
                        )
                    })
            }))
            // The tabs that don't fit, in a menu.
            .when(!hidden.is_empty(), |el| {
                let count = hidden.len();
                el.child(
                    div()
                        .id("tabs-overflow")
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .w(px(TAB_OVERFLOW_WIDTH - 8.))
                        .h(px(24.))
                        .ml_1()
                        .mb(px(4.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .hover_bg(theme.hover)
                        .hover(|s| s.text_color(theme.text))
                        .tooltip(move |_, cx| tooltip_view(format!("{count} more tab{}", plural(count)), cx))
                        .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                            cx.emit(PaneEvent::Overflow { items: hidden.clone(), position: event.position() });
                        }))
                        .child("⋯")
                        .child(count.to_string()),
                )
            })
            // Along the window's top, the free space after the tabs moves the window.
            .when(self.chrome.at_top, |el| {
                el.child(window_drag_area("tabs-free").flex_1().min_w_0().when(cfg!(windows), |el| {
                    el.on_drop(cx.listener(|_, dragged: &DraggedTab, _, cx| {
                        cx.emit(PaneEvent::Drop { item: dragged.item.clone(), before: None });
                    }))
                }))
            })
    }

    /// Under the tabs: the file's folders and name, then (in text) the
    /// definitions around the caret; clicking one goes to it.
    fn render_breadcrumbs(&self, item: &PaneItem, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = item.path(cx)?;
        let symbols = item.editor().map(|editor| editor.update(cx, |e, cx| e.breadcrumbs(cx)));
        let theme = Theme::get(cx).clone();
        let root = self.chrome.root.as_deref();
        let inside = root.is_some_and(|r| path.starts_with(r));
        let rel = display_rel(root, &path);
        let mut parts: Vec<String> = rel.split(['/', '\\']).map(str::to_string).collect();
        let file = parts.pop().unwrap_or_default();
        // A file outside the project: its folder is enough context.
        if !inside {
            parts = parts.pop().into_iter().collect();
        }
        let chevron = || icon_el(Icon::ChevronRight, px(11.), theme.text_dim.into());
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(24.))
            .px_3()
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(theme.background)
            .text_size(px(12.))
            .text_color(theme.text_dim);
        for part in parts {
            row = row.child(div().flex_shrink_0().child(part)).child(chevron());
        }
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(file_icon(&path, px(13.), &theme))
                .child(file),
        );
        let symbols = symbols.unwrap_or_default();
        let last = symbols.len().saturating_sub(1);
        for (ix, crumb) in symbols.into_iter().enumerate() {
            let Some(editor) = item.editor().cloned() else { break };
            let offset = crumb.offset;
            // Colored like the names in the code; plain ones as before.
            let color: Hsla = match crumb.highlight {
                Some(h) => theme.highlight_color(h),
                None if ix == last => theme.text.into(),
                None => theme.text_muted.into(),
            };
            row = row.child(chevron()).child(
                div()
                    .id(("breadcrumb", ix))
                    .flex_shrink_0()
                    .px_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(color)
                    .hover_bg(theme.hover)
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |e, cx| e.go_to(offset, window, cx));
                        window.focus(&editor.focus_handle(cx));
                    })
                    .child(crumb.name),
            );
        }
        Some(row.into_any_element())
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let item = self.tabs.get(self.active).cloned();
        let content: AnyElement = match &item {
            Some(item) => cached_view(item.view()).into_any_element(),
            // An empty pane among others (a lone one shows the workspace's welcome instead).
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .bg(theme.background)
                .text_color(theme.text_dim)
                .child("No open files")
                .child(div().text_size(px(12.)).child("Open a file, or drag a tab here"))
                .into_any_element(),
        };
        // With splits, the pane that has the keyboard is outlined (its current tab too).
        let edge: Hsla = if self.chrome.outlined { crate::ui::focused_edge(&theme) } else { crate::ui::island_edge(&theme) };
        // Find works in text.
        let find_bar = self.chrome.find_bar.clone().filter(|_| item.as_ref().is_some_and(|t| t.editor().is_some()));
        let island = island(&theme)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_color(edge)
            .when_some(item.clone(), |el, item| el.children(self.render_breadcrumbs(&item, cx)))
            .children(find_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    // Clicking into a pane makes it the current one.
                    .capture_any_mouse_down(cx.listener(|pane, _, _, cx| {
                        if !pane.chrome.active {
                            cx.emit(PaneEvent::Focus);
                        }
                    }))
                    .on_drop(cx.listener(|_, dragged: &DraggedTab, _, cx| {
                        cx.emit(PaneEvent::Drop { item: dragged.item.clone(), before: None });
                    }))
                    .child(content),
            );
        let bounds = self.bounds.clone();
        let drop_group = SharedString::from(format!("pane-drop-{}", cx.entity_id()));
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .child(gpui::canvas(move |b, window, _| track_bounds(&bounds, b, window), |_, _, _, _| {}).absolute().size_full())
            // The tab strip's room on the chrome (kept without tabs too, so the
            // island lines up with the sidebar's). Empty along the window's top,
            // it moves the window; with tabs, their strip's free space does.
            .map(|el| {
                let room = px(TAB_STRIP_HEIGHT - 1.);
                if self.tabs.is_empty() && self.chrome.at_top {
                    el.child(window_drag_area("tabs-room").flex_shrink_0().h(room))
                } else {
                    el.child(div().flex_shrink_0().h(room))
                }
            })
            .child(island)
            // After the island, so the current tab paints over its top border.
            .when(!self.tabs.is_empty() && !self.chrome.covered, |el| el.child(self.render_strip(edge, window, cx)))
            // Finder files, and files dragged from the file tree, dropped anywhere
            // over the pane (tabs included) open in it.
            .group(drop_group.clone())
            .on_drag_move(cx.listener(|pane, ev: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                pane.set_file_drop(ev.bounds.contains(&ev.event.position), cx)
            }))
            .on_drag_move(cx.listener(|pane, ev: &gpui::DragMoveEvent<crate::project_tree::DraggedEntry>, _, cx| {
                let over = ev.bounds.contains(&ev.event.position) && !ev.drag(cx).path.is_dir();
                pane.set_file_drop(over, cx)
            }))
            .child(
                file_drop_overlay(drop_group, &theme)
                    .on_drop(cx.listener(|pane, paths: &gpui::ExternalPaths, _, cx| {
                        cx.stop_propagation();
                        pane.set_file_drop(false, cx);
                        cx.emit(PaneEvent::OpenPaths(paths.paths().to_vec()));
                    }))
                    .on_drop(cx.listener(|pane, entry: &crate::project_tree::DraggedEntry, _, cx| {
                        cx.stop_propagation();
                        pane.set_file_drop(false, cx);
                        cx.emit(PaneEvent::OpenPath(entry.path.clone()));
                    })),
            )
    }
}

/// "Drop to open" over a pane while files from Finder or the tree are dragged
/// over it (the `group` the pane's root names). GPUI shows it by its drag-over
/// styles, so there's no state to go stale; being a drop target gives it the
/// hitbox GPUI needs to apply them, and hidden it takes no events. Callers add
/// the drop listeners.
pub(super) fn file_drop_overlay(group: SharedString, theme: &Theme) -> Div {
    div()
        .invisible()
        .group_drag_over::<gpui::ExternalPaths>(group.clone(), |s| s.visible())
        .group_drag_over::<crate::project_tree::DraggedEntry>(group, |s| s.visible())
        // A tree folder isn't opened by dropping it here.
        .can_drop(|dragged, _, _| dragged.downcast_ref::<crate::project_tree::DraggedEntry>().is_none_or(|entry| !entry.path.is_dir()))
        .absolute()
        .inset_0()
        .child(
            // The island lights up a little (inside its outline, below the
            // tab strip's room); no frame of its own.
            div()
                .absolute()
                .top(px(TAB_STRIP_HEIGHT))
                .left(px(1.))
                .right(px(1.))
                .bottom(px(1.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(crate::ui::ISLAND_RADIUS - 1.))
                .bg(crate::ui::drop_lift(theme).0)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded(px(8.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow(soft_shadow(theme, 2))
                        .text_color(theme.text)
                        .child(icon_el(Icon::FileCode, px(15.), theme.accent.into()))
                        .child("Drop to open"),
                ),
        )
}
