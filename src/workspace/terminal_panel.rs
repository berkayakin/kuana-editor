//! The terminal panel: shells in groups side by side, each an island with
//! its own tab strip (like panes). It keeps the groups and draws them; the
//! workspace starts shells and Run tabs in it, saves its layout and answers
//! what it reports (`TerminalPanelEvent`).

use super::*;

/// What the terminal panel asks of the workspace.
pub(super) enum TerminalPanelEvent {
    /// A path in a terminal's output was ⌘-clicked.
    OpenFile { path: PathBuf, line: Option<u32>, column: Option<u32> },
    /// A Run tab's program ended (its output may report problems).
    RunExited(Entity<TerminalView>),
    /// A Run tab started, ended or went (see `Workspace::running`).
    RunsChanged,
    /// Close this tab (its close button, a middle click): asks first while
    /// something runs in it.
    Close(Entity<TerminalView>),
    /// This tab's shell exited: the tab goes.
    Exited(Entity<TerminalView>),
    /// This tab's shell couldn't start.
    Failed(Entity<TerminalView>, String),
    /// A tab was right-clicked.
    TabMenu { terminal: Entity<TerminalView>, position: gpui::Point<Pixels> },
    /// The tabs that don't fit, to pick from.
    Overflow { terminals: Vec<(Entity<TerminalView>, String)>, position: gpui::Point<Pixels> },
    /// A group's + and split buttons.
    New(usize),
    Split(usize),
    /// The panel's close button.
    Hide,
    /// The current tab or group changed.
    Activated,
}

impl gpui::EventEmitter<TerminalPanelEvent> for TerminalPanel {}

pub(super) struct TerminalPanel {
    /// Never empty groups, in order.
    groups: Vec<TerminalGroup>,
    /// The group that has (or last had) the keyboard.
    active: usize,
    /// The project's terminals from last time, not started yet.
    saved: Option<TerminalLayout>,
    /// The panel's bounds last frame, to fit its groups' tabs.
    bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    subscriptions: HashMap<EntityId, Subscription>,
}

impl TerminalPanel {
    pub(super) fn new() -> Self {
        Self { groups: Vec::new(), active: 0, saved: None, bounds: Default::default(), subscriptions: HashMap::new() }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub(super) fn active_group(&self) -> usize {
        self.active
    }

    pub(super) fn set_active_group(&mut self, group: usize) {
        self.active = group.min(self.groups.len().saturating_sub(1));
    }

    /// The current tab of the current group.
    pub(super) fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        let group = self.groups.get(self.active)?;
        group.tabs.get(group.active)
    }

    pub(super) fn all_terminals(&self) -> impl Iterator<Item = &Entity<TerminalView>> {
        self.groups.iter().flat_map(|g| g.tabs.iter())
    }

    /// (group, tab) of a terminal.
    pub(super) fn position(&self, terminal: &Entity<TerminalView>) -> Option<(usize, usize)> {
        self.groups.iter().enumerate().find_map(|(g, group)| group.tabs.iter().position(|t| t == terminal).map(|ix| (g, ix)))
    }

    pub(super) fn tabs(&self, group: usize) -> &[Entity<TerminalView>] {
        self.groups.get(group).map_or(&[], |g| g.tabs.as_slice())
    }

    /// The project's terminals from last time, started when the panel shows.
    pub(super) fn set_saved(&mut self, saved: Option<TerminalLayout>) {
        self.saved = saved;
    }

    pub(super) fn take_saved(&mut self) -> Option<TerminalLayout> {
        self.saved.take()
    }

    /// No terminals, nothing saved (another project).
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.groups.clear();
        self.subscriptions.clear();
        self.active = 0;
        self.saved = None;
        cx.emit(TerminalPanelEvent::RunsChanged);
        cx.notify();
    }

    /// Puts a started terminal where `place` says and makes it current.
    pub(super) fn add(&mut self, terminal: Entity<TerminalView>, place: TerminalPlace, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(&terminal, |_, terminal, event: &TerminalEvent, cx| match event {
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::Exited if terminal.read(cx).run().is_some() => {
                // Its tab's icon shows how it went, also when it isn't the current one.
                cx.notify();
                cx.emit(TerminalPanelEvent::RunExited(terminal));
                cx.emit(TerminalPanelEvent::RunsChanged);
            }
            TerminalEvent::Exited => cx.emit(TerminalPanelEvent::Exited(terminal)),
            TerminalEvent::Failed(error) => cx.emit(TerminalPanelEvent::Failed(terminal, error.clone())),
            TerminalEvent::OpenFile { path, line, column } => {
                cx.emit(TerminalPanelEvent::OpenFile { path: path.clone(), line: *line, column: *column })
            }
        });
        self.subscriptions.insert(terminal.entity_id(), subscription);
        if terminal.read(cx).run().is_some() {
            cx.emit(TerminalPanelEvent::RunsChanged);
        }
        let (group, ix) = match place {
            TerminalPlace::Tab { group, ix } if group < self.groups.len() => {
                let tabs = &mut self.groups[group].tabs;
                let ix = ix.unwrap_or(tabs.len()).min(tabs.len());
                tabs.insert(ix, terminal);
                (group, ix)
            }
            TerminalPlace::GroupAfter(group) if group < self.groups.len() => {
                self.groups.insert(group + 1, TerminalGroup::new(terminal));
                (group + 1, 0)
            }
            // No group to put it in yet.
            _ => {
                self.groups.push(TerminalGroup::new(terminal));
                (self.groups.len() - 1, 0)
            }
        };
        self.groups[group].active = ix;
        self.active = group;
        cx.notify();
    }

    /// Takes a tab out (a group goes with its last tab). Returns where it
    /// was, for a Run tab's replacement to take its place.
    pub(super) fn remove(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) -> Option<(usize, usize)> {
        let (g, ix) = self.position(terminal)?;
        self.subscriptions.remove(&terminal.entity_id());
        if terminal.read(cx).run().is_some() {
            cx.emit(TerminalPanelEvent::RunsChanged);
        }
        let group = &mut self.groups[g];
        group.tabs.remove(ix);
        // The current tab stays current when another one closes.
        if ix < group.active {
            group.active -= 1;
        }
        group.active = group.active.min(group.tabs.len().saturating_sub(1));
        // A group goes with its last tab; the one after it (else before) takes over.
        if group.tabs.is_empty() {
            self.groups.remove(g);
            if self.active > g {
                self.active -= 1;
            }
            self.active = self.active.min(self.groups.len().saturating_sub(1));
        }
        cx.notify();
        Some((g, ix))
    }

    /// Makes a tab current, its group the one with the keyboard, and focuses it.
    pub(super) fn activate(&mut self, group: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.groups.get_mut(group) else { return };
        g.active = ix.min(g.tabs.len().saturating_sub(1));
        self.active = group;
        if let Some(terminal) = self.active_terminal() {
            window.focus(&terminal.focus_handle(cx));
        }
        cx.emit(TerminalPanelEvent::Activated);
        cx.notify();
    }

    /// Restores a saved group's current tab (`restore_terminals`).
    pub(super) fn set_group_active(&mut self, group: usize, ix: usize) {
        if let Some(g) = self.groups.get_mut(group) {
            g.active = ix.min(g.tabs.len().saturating_sub(1));
        }
    }

    /// The panel as the session keeps it: each shell's folder and name, by
    /// group. Run tabs are left out (they'd run their program again).
    pub(super) fn layout(&self, open: bool, cx: &App) -> TerminalLayout {
        if self.groups.is_empty()
            && let Some(saved) = &self.saved
        {
            return TerminalLayout { open, ..saved.clone() };
        }
        let mut groups = Vec::new();
        let mut active = 0;
        for (g, group) in self.groups.iter().enumerate() {
            let mut tabs = Vec::new();
            let mut current = 0;
            for (ix, terminal) in group.tabs.iter().enumerate() {
                let terminal = terminal.read(cx);
                if terminal.run().is_some() {
                    continue;
                }
                // The current tab, else the closest one before it.
                if ix <= group.active {
                    current = tabs.len();
                }
                tabs.push(TerminalTabSession { name: terminal.custom_title().map(|t| t.to_string()), cwd: Some(terminal.current_dir()) });
            }
            if tabs.is_empty() {
                continue;
            }
            if g <= self.active {
                active = groups.len();
            }
            groups.push(TerminalGroupSession { tabs, active: current });
        }
        TerminalLayout { open, groups, active }
    }

    fn render_group(&self, g: usize, count: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let group = self.groups.get(g);
        let current = g == self.active;
        let last = g + 1 == count;
        // With splits, the group that has the keyboard is outlined (its current tab too), like a pane.
        let edge: Hsla = if count > 1 && current {
            crate::ui::focused_edge(&theme)
        } else {
            crate::ui::island_edge(&theme)
        };
        let tabs: Vec<Entity<TerminalView>> = group.map(|g| g.tabs.clone()).unwrap_or_default();
        let active = group.map_or(0, |g| g.active);
        // The strip's room for tabs: the group's share of the panel, less the
        // strip's insets and its buttons (+, split, and hide on the last group).
        let buttons = TERMINAL_BUTTON * if last { 3. } else { 2. } + 8.;
        let total = f32::from(self.bounds.get().size.width);
        let (gap, inset) = (layout_gap(&theme), tab_strip_inset(&theme));
        let available = (total > 0.).then(|| (total - gap * (count - 1) as f32) / count as f32 - 2. * inset - buttons);
        let names: Vec<String> = tabs.iter().map(|t| t.read(cx).title().to_string()).collect();
        let name_widths = measure_tab_names(&names, window, &theme);
        let naturals: Vec<f32> = name_widths.iter().map(|(w, _)| (TAB_CHROME + w).ceil()).collect();
        let slots = fit_tabs(&naturals, active, available.unwrap_or(f32::INFINITY));
        let visible: Vec<usize> = (0..slots.len()).filter(|&i| slots[i] != TabSlot::Hidden).collect();
        let hidden: Vec<(Entity<TerminalView>, String)> =
            (0..slots.len()).filter(|&i| slots[i] == TabSlot::Hidden).map(|i| (tabs[i].clone(), names[i].clone())).collect();

        let tab_elements = visible.iter().enumerate().map(|(pos, &ix)| {
            let terminal = &tabs[ix];
            let slot = slots[ix];
            let is_active = ix == active;
            let separator = !is_active && visible.get(pos + 1).is_some_and(|&n| n != active);
            let after_active = pos > 0 && visible[pos - 1] == active;
            let icon_only = matches!(slot, TabSlot::Icon(_));
            let compact = matches!(slot, TabSlot::Compact(_));
            let hover_group = SharedString::from(format!("terminal-tab-{g}-{ix}"));
            let (for_close, for_middle, for_menu) = (terminal.clone(), terminal.clone(), terminal.clone());
            let label = tab_label(&names[ix], slot, naturals[ix], name_widths[ix]);
            let name_color: Hsla = if is_active && current { theme.text.into() } else { theme.text_muted.into() };
            let tab = div()
                .id(SharedString::from(format!("terminal-tab-{g}-{ix}")))
                .group(hover_group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h_full()
                .cursor_pointer();
            tab_slot_width(tab_shape(tab, is_active, after_active, separator, hover_group.clone(), edge, &theme), slot, naturals[ix])
                .when(icon_only || compact, |el| {
                    let name = names[ix].clone();
                    el.tooltip(move |_, cx| tooltip_view(name.clone(), cx))
                })
                .on_click(cx.listener(move |panel, _, window, cx| panel.activate(g, ix, window, cx)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |_, _, _, cx| cx.emit(TerminalPanelEvent::Close(for_middle.clone()))),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(TerminalPanelEvent::TabMenu { terminal: for_menu.clone(), position: event.position });
                    }),
                )
                .child(match terminal.read(cx).run().map(|(_, state)| *state) {
                    // A Run tab shows how its program is doing.
                    Some(RunState::Running) => icon_el(Icon::Play, px(13.), theme.success.into()),
                    Some(RunState::Exited(Some(0))) => icon_el(Icon::CircleCheck, px(13.), theme.success.into()),
                    Some(RunState::Exited(_)) => icon_el(Icon::CircleX, px(13.), theme.error.into()),
                    None => icon_el(Icon::Terminal, px(13.), if is_active { theme.accent.into() } else { theme.text_dim.into() }),
                })
                .when(!icon_only, |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(name_color)
                            .when(is_active, |el| el.font_weight(FontWeight::MEDIUM))
                            .when(!is_active, |el| el.group_hover(hover_group.clone(), |s| s.text_color(theme.text)))
                            .child(label),
                    )
                })
                .when(!icon_only && !compact, |el| {
                    el.child(
                        div()
                            .id(SharedString::from(format!("close-terminal-{g}-{ix}")))
                            .size(px(18.))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .when(!is_active, |el| el.invisible())
                            .group_hover(hover_group.clone(), |s| s.visible())
                            .hover_bg(theme.active)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(TerminalPanelEvent::Close(for_close.clone()));
                            }))
                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                    )
                })
                .into_any_element()
        });
        // Buttons on the chrome, centered on the tabs.
        let button = |button: Button| button.flex_shrink_0().size(px(TERMINAL_BUTTON - 2.)).mb(px(strip_button_margin(&theme))).mr(px(2.));
        let strip = div()
            .id(("terminal-tabs", g))
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .items_end()
            .pt(px(tab_strip_top(&theme)))
            .pl(px(inset))
            // The buttons' own margins make up the rest.
            .pr(px((inset - 6.).max(2.)))
            .overflow_hidden()
            .children(tab_elements)
            // The tabs that don't fit, in a menu.
            .when(!hidden.is_empty(), |el| {
                let count = hidden.len();
                el.child(
                    div()
                        .id(("terminal-overflow", g))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .w(px(TAB_OVERFLOW_WIDTH - 8.))
                        .h(px(24.))
                        .ml_1()
                        .mb(px(strip_button_margin(&theme)))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .hover_bg(theme.hover)
                        .hover(|s| s.text_color(theme.text))
                        .tooltip(move |_, cx| tooltip_view(format!("{count} more terminal{}", plural(count)), cx))
                        .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                            cx.emit(TerminalPanelEvent::Overflow { terminals: hidden.clone(), position: event.position() });
                        }))
                        .child("⋯")
                        .child(count.to_string()),
                )
            })
            .child(
                button(panel_button("new-terminal", Icon::Plus, cx).on_click(cx.listener(move |_, _, _, cx| cx.emit(TerminalPanelEvent::New(g)))))
                .id(("new-terminal", g))
                .ml_1()
                .tooltip(|window, cx| status_tooltip("New Terminal", &NewTerminal, window, cx)),
            )
            .child(div().flex_1())
            .child(
                button(panel_button("split-terminal", Icon::SplitRight, cx).on_click(cx.listener(move |_, _, _, cx| cx.emit(TerminalPanelEvent::Split(g)))))
                .id(("split-terminal", g))
                .tooltip(|window, cx| status_tooltip("Split Terminal", &SplitTerminal, window, cx)),
            )
            // The panel's own close button, on the group at its right end.
            .when(last, |el| {
                el.child(
                    button(panel_button("hide-terminal", Icon::X, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalPanelEvent::Hide))))
                    .tooltip(|window, cx| status_tooltip("Hide Terminal", &ToggleTerminal, window, cx)),
                )
            });
        div()
            .id(("terminal-group", g))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            // Clicking into a group makes it the current one.
            .capture_any_mouse_down(cx.listener(move |panel, _, _, cx| {
                if panel.active != g && g < panel.groups.len() {
                    panel.active = g;
                    cx.notify();
                }
            }))
            .child(tab_room(div(), &theme))
            .child(
                area(&theme)
                    .flex_1()
                    .min_h_0()
                    .border_color(edge)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .children(tabs.get(active).map(|t| div().flex_1().min_w_0().child(cached(t)))),
                    ),
            )
            // After the island, so the current tab paints over its top border.
            .child(strip)
            .into_any_element()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A panel reopened at launch gets its shell in `window_ready`; until
        // then it's one empty group.
        let count = self.groups.len().max(1);
        let bounds = self.bounds.clone();
        div()
            .relative()
            .flex()
            .gap(px(layout_gap(Theme::get(cx))))
            .size_full()
            .child(gpui::canvas(move |b, window, _| track_bounds(&bounds, b, window), |_, _, _, _| {}).absolute().size_full())
            .children((0..count).map(|g| self.render_group(g, count, window, cx)))
    }
}

/// A button on a group's tab strip.
fn panel_button(id: &'static str, icon: Icon, cx: &App) -> Button {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover_bg(theme.hover)
        .active(|s| s.bg(theme.active))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon_el(icon, px(15.), theme.text_dim.into()))
}
