//! The Find panel (JetBrains' Find tool window): results as trees, one tab
//! per search, in the bottom area. Find Usages fills it with the usages
//! grouped by file, Call / Type Hierarchy with trees that load as they
//! open (`hierarchy.rs`); nodes are generic (`ResultNode`: a label, an
//! optional place to open, children or a `Loader` that fetches them the
//! first time the node opens), so other trees can show here too.
//!
//! A new search replaces the current tab unless it's pinned. ↑/↓ move and
//! show the selected place in its editor (the panel keeps focus), Enter opens
//! it, ←/→ collapse and expand, Escape goes back to the editor; Next / Previous
//! Result step through the current tab from anywhere. The workspace opens
//! what's chosen (`ResultsEvent::Open`) and draws the island around it.

use std::{ops::Range, path::PathBuf, rc::Rc};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, Render, ScrollStrategy, SharedString, StyledText, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    lsp::protocol::{Encoding, Location, Position},
    theme::Theme,
    ui::{HoverFill, icon_el},
};

/// A place to open, as a language server gave it (its column resolves
/// against the file's text when opened).
#[derive(Clone, Debug, PartialEq)]
pub struct ResultTarget {
    pub path: PathBuf,
    pub position: Position,
    pub encoding: Encoding,
}

/// What a row shows before its label.
#[derive(Clone, Debug, PartialEq)]
pub enum Leading {
    /// A file: its icon.
    File(PathBuf),
    /// A line in a file, 1-based.
    Line(u32),
    Icon(Icon),
    /// An icon in a color that means something (a test's pass or fail).
    Tinted(Icon, Tone),
    /// Nothing ("Loading…", a message).
    Blank,
}

/// What a tinted icon's color says; the theme picks the color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Success,
    Error,
    Muted,
}

/// A button in the panel's header while its tab is current (Rerun Failed).
#[derive(Clone)]
pub struct TabButton {
    pub icon: Icon,
    pub label: SharedString,
    pub on_click: Rc<dyn Fn(&mut Window, &mut App)>,
}

/// What a node's children take to load: the workspace or a hierarchy hands
/// one over, the panel runs it once, the first time the node opens.
pub type LoadTask = Task<Result<Vec<ResultNode>, String>>;

#[derive(Clone)]
pub struct Loader(Rc<dyn Fn(&mut App) -> LoadTask>);

impl Loader {
    pub fn new(load: impl Fn(&mut App) -> LoadTask + 'static) -> Self {
        Loader(Rc::new(load))
    }
}

impl PartialEq for Loader {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Loader")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResultNode {
    pub leading: Leading,
    pub label: SharedString,
    /// Bytes of `label` to highlight (the usage in its line).
    pub highlight: Option<Range<usize>>,
    /// Dim text after the label (a file's folder, a count).
    pub detail: Option<SharedString>,
    pub target: Option<ResultTarget>,
    pub children: Vec<ResultNode>,
    /// Fetches the children (until it has run).
    pub loader: Option<Loader>,
    pub expanded: bool,
}

impl ResultNode {
    /// A row without a place or children: "Loading…", "None", an error.
    pub fn message(text: impl Into<SharedString>) -> Self {
        ResultNode {
            leading: Leading::Blank,
            label: SharedString::default(),
            highlight: None,
            detail: Some(text.into()),
            target: None,
            children: Vec::new(),
            loader: None,
            expanded: false,
        }
    }

    fn expandable(&self) -> bool {
        !self.children.is_empty() || self.loader.is_some()
    }
}

/// A row on screen: a node by its index path from the tab's roots.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    path: Vec<usize>,
    depth: usize,
}

struct ResultTab {
    /// Tells a load's results which tab they're for (tabs get replaced).
    id: u64,
    title: SharedString,
    /// Leaves with a place, shown next to the title; none for a tree that
    /// loads as it opens.
    count: Option<usize>,
    nodes: Vec<ResultNode>,
    /// The expanded nodes, depth first.
    rows: Vec<Row>,
    selected: Option<usize>,
    pinned: bool,
    buttons: Vec<TabButton>,
}

impl ResultTab {
    fn new(id: u64, title: SharedString, nodes: Vec<ResultNode>) -> Self {
        fn leaves(nodes: &[ResultNode]) -> Option<usize> {
            nodes.iter().try_fold(0, |n, node| {
                if node.loader.is_some() {
                    None
                } else if node.children.is_empty() {
                    Some(n + node.target.is_some() as usize)
                } else {
                    Some(n + leaves(&node.children)?)
                }
            })
        }
        let mut tab = ResultTab { id, title, count: leaves(&nodes), nodes, rows: Vec::new(), selected: None, pinned: false, buttons: Vec::new() };
        tab.relayout();
        tab
    }

    fn node(&self, path: &[usize]) -> &ResultNode {
        let mut node = &self.nodes[path[0]];
        for &i in &path[1..] {
            node = &node.children[i];
        }
        node
    }

    fn node_mut(&mut self, path: &[usize]) -> &mut ResultNode {
        let mut node = &mut self.nodes[path[0]];
        for &i in &path[1..] {
            node = &mut node.children[i];
        }
        node
    }

    /// Lists the rows again after nodes opened or closed, keeping the
    /// selected node selected (or its closed parent).
    fn relayout(&mut self) {
        fn walk(nodes: &[ResultNode], prefix: &mut Vec<usize>, rows: &mut Vec<Row>) {
            for (i, node) in nodes.iter().enumerate() {
                prefix.push(i);
                rows.push(Row { path: prefix.clone(), depth: prefix.len() - 1 });
                if node.expanded {
                    walk(&node.children, prefix, rows);
                }
                prefix.pop();
            }
        }
        let selected = self.selected.and_then(|s| self.rows.get(s)).map(|r| r.path.clone());
        self.rows.clear();
        walk(&self.nodes, &mut Vec::new(), &mut self.rows);
        self.selected = selected.and_then(|mut path| {
            while !path.is_empty() {
                if let Some(ix) = self.rows.iter().position(|r| r.path == path) {
                    return Some(ix);
                }
                path.pop();
            }
            None
        });
    }

    /// Opens or closes a row's node; one opening for the first time with
    /// children to load gives its loader back (to run) and shows "Loading…".
    fn set_expanded(&mut self, row: usize, expanded: bool) -> Option<(Vec<usize>, Loader)> {
        let path = self.rows[row].path.clone();
        let node = self.node_mut(&path);
        if !node.expandable() || node.expanded == expanded {
            return None;
        }
        node.expanded = expanded;
        let load = expanded.then(|| node.loader.take()).flatten().map(|loader| {
            node.children = vec![ResultNode::message("Loading…")];
            (path, loader)
        });
        self.relayout();
        load
    }

    /// The loaders of nodes that show open without their children yet (a
    /// hierarchy's first levels), taken to run.
    fn take_open_loaders(&mut self) -> Vec<(Vec<usize>, Loader)> {
        fn walk(nodes: &mut [ResultNode], prefix: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, Loader)>) {
            for (i, node) in nodes.iter_mut().enumerate() {
                prefix.push(i);
                if node.expanded {
                    if let Some(loader) = node.loader.take() {
                        node.children = vec![ResultNode::message("Loading…")];
                        out.push((prefix.clone(), loader));
                    }
                    walk(&mut node.children, prefix, out);
                }
                prefix.pop();
            }
        }
        let mut out = Vec::new();
        walk(&mut self.nodes, &mut Vec::new(), &mut out);
        self.relayout();
        out
    }

    /// A load finished: its node's children, or what went wrong.
    fn loaded(&mut self, path: &[usize], result: Result<Vec<ResultNode>, String>) {
        let node = self.node_mut(path);
        node.children = match result {
            Ok(children) if children.is_empty() => vec![ResultNode::message("None")],
            Ok(children) => children,
            Err(err) => vec![ResultNode::message(err)],
        };
        self.relayout();
    }

    /// Expand All opens what's loaded; it doesn't start loads.
    fn set_all_expanded(&mut self, expanded: bool) {
        fn set(nodes: &mut [ResultNode], expanded: bool) {
            for node in nodes {
                node.expanded = expanded && !node.children.is_empty();
                set(&mut node.children, expanded);
            }
        }
        set(&mut self.nodes, expanded);
        self.relayout();
    }

    /// The next (or previous) leaf with a place after the selection, in
    /// tree order, opening the nodes on the way.
    fn step_leaf(&mut self, forward: bool) -> Option<ResultTarget> {
        fn leaves(nodes: &[ResultNode], prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
            for (i, node) in nodes.iter().enumerate() {
                prefix.push(i);
                if node.children.is_empty() {
                    if node.target.is_some() {
                        out.push(prefix.clone());
                    }
                } else {
                    leaves(&node.children, prefix, out);
                }
                prefix.pop();
            }
        }
        let mut all = Vec::new();
        leaves(&self.nodes, &mut Vec::new(), &mut all);
        let current = self.selected.and_then(|s| self.rows.get(s)).map(|r| r.path.clone());
        let next = match current {
            None if forward => all.first(),
            None => all.last(),
            // Paths compare in tree order; a group comes before its leaves.
            Some(current) if forward => all.iter().find(|p| **p > current),
            Some(current) => all.iter().rev().find(|p| **p < current),
        }?
        .clone();
        for depth in 1..next.len() {
            self.node_mut(&next[..depth]).expanded = true;
        }
        self.relayout();
        self.selected = self.rows.iter().position(|r| r.path == next);
        self.node(&next).target.clone()
    }
}

pub enum ResultsEvent {
    /// Show this place; `focus`: move to the editor (else the panel keeps it).
    Open { target: ResultTarget, focus: bool },
    /// Back to the editor (Escape).
    Leave,
    Close,
    /// The last tab closed.
    Emptied,
}

impl EventEmitter<ResultsEvent> for ResultsPanel {}

/// How many searches stay; the oldest unpinned goes first.
const MAX_TABS: usize = 10;
const ROW_HEIGHT: f32 = 24.;

pub struct ResultsPanel {
    tabs: Vec<ResultTab>,
    active: usize,
    next_id: u64,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl ResultsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { tabs: Vec::new(), active: 0, next_id: 0, focus: cx.focus_handle(), scroll: UniformListScrollHandle::new() }
    }

    /// Shows a search's results: in place of the current tab unless that's
    /// pinned; `buttons` show in the header while it's current. `counted`:
    /// the tab shows how many places it lists (a test run's title says how
    /// it went instead).
    pub fn show(&mut self, title: SharedString, nodes: Vec<ResultNode>, buttons: Vec<TabButton>, counted: bool, cx: &mut Context<Self>) {
        self.next_id += 1;
        let mut tab = ResultTab::new(self.next_id, title, nodes);
        tab.buttons = buttons;
        if !counted {
            tab.count = None;
        }
        let loads = tab.take_open_loaders();
        match self.tabs.get(self.active) {
            Some(current) if !current.pinned => self.tabs[self.active] = tab,
            _ => {
                if self.tabs.len() >= MAX_TABS
                    && let Some(oldest) = self.tabs.iter().position(|t| !t.pinned)
                {
                    self.tabs.remove(oldest);
                }
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
        }
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        for (path, loader) in loads {
            self.load(self.next_id, path, loader, cx);
        }
        cx.notify();
    }

    /// Runs a node's loader and puts its children in, if its tab is still there.
    fn load(&mut self, tab: u64, path: Vec<usize>, loader: Loader, cx: &mut Context<Self>) {
        let task = (loader.0)(cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab) {
                    tab.loaded(&path, result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Opens or closes a row of the current tab, loading what it needs.
    fn expand(&mut self, row: usize, expanded: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let id = tab.id;
        if let Some((path, loader)) = tab.set_expanded(row, expanded) {
            self.load(id, path, loader, cx);
        }
        cx.notify();
    }

    fn tab(&mut self) -> Option<&mut ResultTab> {
        self.tabs.get_mut(self.active)
    }

    /// Next / Previous Result: selects it and says where it is.
    pub fn step_result(&mut self, forward: bool, cx: &mut Context<Self>) -> Option<ResultTarget> {
        let target = self.tab()?.step_leaf(forward);
        self.reveal_selected();
        cx.notify();
        target
    }

    fn reveal_selected(&self) {
        if let Some(selected) = self.tabs.get(self.active).and_then(|t| t.selected) {
            self.scroll.scroll_to_item(selected, ScrollStrategy::Center);
        }
    }

    fn select(&mut self, row: usize, open: Option<bool>, cx: &mut Context<Self>) {
        let Some(tab) = self.tab() else { return };
        tab.selected = Some(row);
        let target = tab.rows.get(row).and_then(|r| tab.node(&r.path).target.clone());
        self.reveal_selected();
        if let (Some(target), Some(focus)) = (target, open) {
            cx.emit(ResultsEvent::Open { target, focus });
        }
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.rows.is_empty() {
            return;
        }
        let next = match tab.selected {
            Some(s) => (s as isize + delta).clamp(0, tab.rows.len() as isize - 1) as usize,
            None => 0,
        };
        // Arrows show the place without leaving the panel.
        self.select(next, Some(false), cx);
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    /// ←: close the selected node, else go to its parent.
    fn on_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab() else { return };
        let Some(row) = tab.selected else { return };
        let path = tab.rows[row].path.clone();
        if tab.node(&path).expanded {
            tab.set_expanded(row, false);
        } else if path.len() > 1 {
            let parent = &path[..path.len() - 1];
            tab.selected = tab.rows.iter().position(|r| r.path == parent);
        }
        self.reveal_selected();
        cx.notify();
    }

    /// →: open the selected node, else go to its first child.
    fn on_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab() else { return };
        let Some(row) = tab.selected else { return };
        let path = tab.rows[row].path.clone();
        let node = tab.node(&path);
        if !node.expandable() {
            return;
        }
        if node.expanded {
            tab.selected = Some(row + 1);
        } else {
            self.expand(row, true, cx);
        }
        self.reveal_selected();
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab() else { return };
        let Some(row) = tab.selected else { return };
        let path = tab.rows[row].path.clone();
        let node = tab.node(&path);
        if node.target.is_none() && node.expandable() {
            let expanded = node.expanded;
            self.expand(row, !expanded, cx);
        } else {
            self.select(row, Some(true), cx);
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ResultsEvent::Leave);
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.tabs.is_empty() {
            cx.emit(ResultsEvent::Emptied);
        }
        cx.notify();
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div().flex().items_center().gap_1().min_w_0().overflow_hidden().children(self.tabs.iter().enumerate().map(|(ix, tab)| {
            let active = ix == self.active;
            let (text, rest) = if active { (theme.accent, Hsla::from(theme.accent_soft)) } else { (theme.text_muted, Hsla::from(theme.accent_soft).alpha(0.)) };
            div()
                .id(("results-tab", ix))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .h(px(24.))
                .pl_2()
                .pr_1()
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(text)
                .when(tab.pinned, |el| el.child(icon_el(Icon::Pin, px(11.), text.into())))
                .child(div().max_w(px(260.)).truncate().child(tab.title.clone()))
                .children(tab.count.map(|n| div().text_color(theme.text_dim).child(n.to_string())))
                .child(
                    div()
                        .id(("results-tab-close", ix))
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .hover_bg(theme.hover)
                        .child(icon_el(Icon::X, px(11.), theme.text_dim.into()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_tab(ix, cx);
                        })),
                )
                .hover_fill(rest, theme.hover)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = ix;
                    cx.notify();
                }))
        }))
    }

    fn render_row(&self, tab: &ResultTab, ix: usize, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::get(cx);
        let row = &tab.rows[ix];
        let node = tab.node(&row.path);
        let selected = tab.selected == Some(ix);
        let chevron = if node.expandable() {
            let expanded = node.expanded;
            div()
                .id(("results-chevron", ix))
                .flex_shrink_0()
                .child(icon_el(if expanded { Icon::ChevronDown } else { Icon::ChevronRight }, px(14.), theme.text_dim.into()))
                // Opens or closes without going to the node's place.
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus);
                    this.select(ix, None, cx);
                    this.expand(ix, !expanded, cx);
                }))
                .into_any_element()
        } else {
            div().size(px(14.)).flex_shrink_0().into_any_element()
        };
        let leading = match &node.leading {
            Leading::File(path) => crate::ui::file_icon(path, px(14.), theme).into_any_element(),
            Leading::Line(line) => div()
                .min_w(px(36.))
                .flex_shrink_0()
                .text_right()
                .text_size(px(12.))
                .text_color(theme.text_dim)
                .child(line.to_string())
                .into_any_element(),
            Leading::Icon(icon) => icon_el(*icon, px(14.), theme.text_dim.into()).into_any_element(),
            Leading::Tinted(icon, tone) => {
                let color = match tone {
                    Tone::Success => theme.success,
                    Tone::Error => theme.error,
                    Tone::Muted => theme.text_dim,
                };
                icon_el(*icon, px(14.), color.into()).into_any_element()
            }
            Leading::Blank => div().into_any_element(),
        };
        let label = match &node.highlight {
            Some(range) if range.end <= node.label.len() => StyledText::new(node.label.clone())
                .with_highlights([(
                    range.clone(),
                    HighlightStyle { background_color: Some(theme.search_match.into()), color: Some(theme.text.into()), ..Default::default() },
                )])
                .into_any_element(),
            _ => StyledText::new(node.label.clone()).into_any_element(),
        };
        let code = matches!(node.leading, Leading::Line(_));
        let selected_bg = if focused { theme.accent_soft } else { theme.active };
        div()
            .id(("results-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .gap_1p5()
            .pl(px(8. + 16. * row.depth as f32))
            .pr_3()
            .cursor_pointer()
            .when(selected, |el| el.bg(selected_bg))
            .row_hover(theme.hover)
            .child(chevron)
            .child(leading)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .when(code, |el| el.font_family(theme.mono_font.clone()).text_size(px(12.5)))
                    .child(label),
            )
            .children(node.detail.clone().map(|d| div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(d)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus);
                let Some(tab) = this.tabs.get(this.active) else { return };
                let (has_target, expanded) = {
                    let node = tab.node(&tab.rows[ix].path);
                    (node.target.is_some(), node.expanded)
                };
                if !has_target || event.click_count() > 1 {
                    this.expand(ix, !expanded, cx);
                }
                this.select(ix, Some(has_target), cx);
            }))
            .into_any_element()
    }
}

impl Focusable for ResultsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ResultsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let pinned = self.tabs.get(self.active).is_some_and(|t| t.pinned);
        let header = crate::ui::panel_header(&theme)
            .child(div().flex_shrink_0().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child("FIND"))
            .child(self.render_tabs(cx))
            .child(div().flex_1())
            .children(self.tabs.get(self.active).into_iter().flat_map(|t| t.buttons.clone()).enumerate().map(|(ix, button)| {
                let on_click = button.on_click.clone();
                div()
                    .id(("results-button", ix))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .h(px(24.))
                    .px_2()
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .hover_bg(theme.hover)
                    .child(icon_el(button.icon, px(13.), theme.text_muted.into()))
                    .child(button.label.clone())
                    .on_click(move |_, window, cx| on_click(window, cx))
            }))
            .when(!self.tabs.is_empty(), |el| {
                el.child(
                    crate::ui::icon_button("results-pin", Icon::Pin, if pinned { theme.accent } else { theme.text_dim }.into(), cx)
                        .tooltip(move |_, cx| crate::ui::tooltip_view(if pinned { "Unpin Tab" } else { "Pin Tab (the next search opens a new tab)" }, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(tab) = this.tab() {
                                tab.pinned = !tab.pinned;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    crate::ui::icon_button("results-expand", Icon::ChevronDown, theme.text_dim.into(), cx)
                        .tooltip(|_, cx| crate::ui::tooltip_view("Expand All", cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(tab) = this.tab() {
                                tab.set_all_expanded(true);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    crate::ui::icon_button("results-collapse", Icon::ChevronsDownUp, theme.text_dim.into(), cx)
                        .tooltip(|_, cx| crate::ui::tooltip_view("Collapse All", cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(tab) = this.tab() {
                                tab.set_all_expanded(false);
                            }
                            cx.notify();
                        })),
                )
            })
            .child(crate::ui::panel_close_button("close-results", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(ResultsEvent::Close))));

        let rows = self.tabs.get(self.active).map_or(0, |t| t.rows.len());
        let list = if rows == 0 {
            div().p_3().text_color(theme.text_dim).child("Find Usages lists a symbol's usages here.").into_any_element()
        } else {
            let focused = self.focus.contains_focused(window, cx);
            uniform_list(
                "results-list",
                rows,
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let Some(tab) = this.tabs.get(this.active) else { return Vec::new() };
                    range.filter(|&ix| ix < tab.rows.len()).map(|ix| this.render_row(tab, ix, focused, cx)).collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .size_full()
            .py_1()
            .into_any_element()
        };
        div()
            .key_context("Results")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(div().relative().flex_1().min_h_0().child(list).child(crate::ui::list_scrollbar("results-scrollbar", &self.scroll, cx)))
    }
}

/// A usage's line as the panel shows it: trimmed, and cut before a usage far
/// along a long line, with the usage's bytes in the result.
fn preview(line: &str, usage: Range<usize>) -> (String, Range<usize>) {
    const LEAD: usize = 40;
    let line = line.trim_end_matches(['\n', '\r']);
    let start = line.len() - line.trim_start().len();
    let usage = usage.start.clamp(start, line.len())..usage.end.clamp(start, line.len());
    let mut from = start;
    if usage.start - from > LEAD * 2 {
        from = usage.start - LEAD;
        while !line.is_char_boundary(from) {
            from -= 1;
        }
    }
    let ellipsis = if from > start { "…" } else { "" };
    let shift = ellipsis.len() as isize - from as isize;
    let text = format!("{ellipsis}{}", line[from..].trim_end());
    let at = |b: usize| ((b as isize + shift).max(0) as usize).min(text.len());
    (text.clone(), at(usage.start)..at(usage.end))
}

/// The byte column of an LSP character in `line`.
fn column(line: &str, character: u32, encoding: Encoding) -> usize {
    let rope = ropey::Rope::from_str(line);
    crate::lsp::protocol::position_to_offset(&rope, Position { line: 0, character }, encoding)
}

/// Usages grouped by file (files in path order, usages by place), each with
/// its line (`lines[i]` for `locations[i]`, as it reads today). `label`
/// names a file as the panel shows it (relative to the project).
pub fn usages_tree(locations: Vec<Location>, lines: Vec<String>, encoding: Encoding, label: impl Fn(&std::path::Path) -> String) -> Vec<ResultNode> {
    let mut usages: Vec<(Location, String)> = locations.into_iter().zip(lines).collect();
    usages.sort_by(|(a, _), (b, _)| (&a.path, a.range.start.line, a.range.start.character).cmp(&(&b.path, b.range.start.line, b.range.start.character)));
    usages.dedup_by(|(a, _), (b, _)| a == b);
    let mut files: Vec<ResultNode> = Vec::new();
    for (location, line) in usages {
        let start = column(&line, location.range.start.character, encoding);
        let end = if location.range.end.line == location.range.start.line {
            column(&line, location.range.end.character, encoding).max(start)
        } else {
            line.trim_end().len()
        };
        let (text, highlight) = preview(&line, start..end);
        let usage = ResultNode {
            leading: Leading::Line(location.range.start.line + 1),
            label: text.into(),
            highlight: Some(highlight),
            detail: None,
            target: Some(ResultTarget { path: location.path.clone(), position: location.range.start, encoding }),
            children: Vec::new(),
            loader: None,
            expanded: false,
        };
        match files.last_mut() {
            Some(file) if matches!(&file.leading, Leading::File(p) if *p == location.path) => file.children.push(usage),
            _ => {
                let shown = label(&location.path);
                let (folder, name) = match shown.rsplit_once('/') {
                    Some((folder, name)) => (Some(folder.to_string()), name.to_string()),
                    None => (None, shown),
                };
                files.push(ResultNode {
                    leading: Leading::File(location.path.clone()),
                    label: name.into(),
                    highlight: None,
                    detail: folder.map(Into::into),
                    target: None,
                    children: vec![usage],
                    loader: None,
                    expanded: true,
                });
            }
        }
    }
    for file in &mut files {
        let n = file.children.len();
        let count = format!("{n} usage{}", if n == 1 { "" } else { "s" });
        file.detail = Some(match &file.detail {
            Some(folder) => format!("{folder}  ·  {count}").into(),
            None => count.into(),
        });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::protocol::LspRange;

    fn at(path: &str, line: u32, start: u32, end: u32) -> Location {
        Location {
            path: PathBuf::from(path),
            range: LspRange { start: Position { line, character: start }, end: Position { line, character: end } },
        }
    }

    #[test]
    fn usages_group_by_file_in_order() {
        let locations = vec![at("/p/src/b.rs", 9, 8, 11), at("/p/src/a.rs", 3, 4, 7), at("/p/src/b.rs", 1, 0, 3), at("/p/src/a.rs", 3, 4, 7)];
        let lines = vec!["        foo();".into(), "    foo".into(), "foo".into(), "    foo".into()];
        let tree = usages_tree(locations, lines, Encoding::Utf8, |p| p.strip_prefix("/p").unwrap().display().to_string().trim_start_matches('/').to_string());
        assert_eq!(tree.len(), 2);
        assert_eq!((tree[0].label.as_ref(), tree[0].detail.as_ref().map(|d| d.as_ref())), ("a.rs", Some("src  ·  1 usage")));
        assert_eq!(tree[1].children.len(), 2);
        let first = &tree[1].children[0];
        assert_eq!((first.leading.clone(), first.label.as_ref()), (Leading::Line(2), "foo"));
        let second = &tree[1].children[1];
        assert_eq!((second.label.as_ref(), second.highlight.clone()), ("foo();", Some(0..3)));
        assert_eq!(second.target, Some(ResultTarget { path: "/p/src/b.rs".into(), position: Position { line: 9, character: 8 }, encoding: Encoding::Utf8 }));
    }

    #[test]
    fn previews_cut_long_lines_and_count_utf16() {
        let line = format!("{}target", "x".repeat(200));
        let (text, range) = preview(&line, 200..206);
        assert!(text.starts_with('…') && text.len() < 60);
        assert_eq!(&text[range], "target");
        // "é" is one UTF-16 unit and two bytes.
        assert_eq!(column("é foo", 2, Encoding::Utf16), 3);
    }

    #[test]
    fn lazy_nodes_load_once_when_opened() {
        let lazy = |label: &str, expanded| ResultNode {
            leading: Leading::Blank,
            label: label.to_string().into(),
            loader: Some(Loader::new(|_| Task::ready(Ok(Vec::new())))),
            expanded,
            ..ResultNode::message("")
        };
        let mut tab = ResultTab::new(1, "t".into(), vec![lazy("open", true), lazy("closed", false)]);
        // A tree that loads has no count; what shows open loads at once.
        assert_eq!(tab.count, None);
        let first = tab.take_open_loaders();
        assert_eq!(first.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), [vec![0]]);
        assert_eq!(tab.rows.len(), 3, "Loading… under the open one");
        tab.loaded(&[0], Ok(Vec::new()));
        assert_eq!(tab.node(&[0, 0]).detail.as_ref().map(|d| d.as_ref()), Some("None"));
        // The closed one loads when it opens, only the first time.
        assert!(tab.set_expanded(2, true).is_some());
        tab.loaded(&[1], Err("server error".into()));
        assert_eq!(tab.node(&[1, 0]).detail.as_ref().map(|d| d.as_ref()), Some("server error"));
        tab.set_expanded(2, false);
        assert!(tab.set_expanded(2, true).is_none());
    }

    #[test]
    fn stepping_goes_through_leaves_and_opens_groups() {
        let lines = vec!["a".into(), "a".into(), "a".into()];
        let mut nodes = usages_tree(vec![at("/x", 0, 0, 1), at("/x", 1, 0, 1), at("/y", 0, 0, 1)], lines, Encoding::Utf8, |p| p.display().to_string());
        nodes[1].expanded = false;
        let mut tab = ResultTab::new(1, "t".into(), nodes);
        assert_eq!(tab.count, Some(3));
        assert_eq!(tab.rows.len(), 4);
        let lines_of = |t: Option<ResultTarget>| t.map(|t| (t.path, t.position.line));
        assert_eq!(lines_of(tab.step_leaf(true)), Some(("/x".into(), 0)));
        assert_eq!(lines_of(tab.step_leaf(true)), Some(("/x".into(), 1)));
        // Into the closed group, which opens.
        assert_eq!(lines_of(tab.step_leaf(true)), Some(("/y".into(), 0)));
        assert_eq!(tab.rows.len(), 5);
        assert_eq!(tab.step_leaf(true), None);
        assert_eq!(lines_of(tab.step_leaf(false)), Some(("/x".into(), 1)));
        // Closing a group keeps the selection on it.
        tab.set_expanded(0, false);
        assert_eq!(tab.selected, Some(0));
    }
}
