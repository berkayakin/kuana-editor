//! The Find panel (JetBrains' Find tool window): results as trees, one tab
//! per search, in the bottom area. Find Usages fills it with the usages
//! grouped by file; nodes are generic (`ResultNode`: a label, an optional
//! place to open, children), so other trees can show here too.
//!
//! A new search replaces the current tab unless it's pinned. ↑/↓ move and
//! show the selected place in its editor (the panel keeps focus), Enter opens
//! it, ←/→ collapse and expand, Escape goes back to the editor; Next / Previous
//! Result step through the current tab from anywhere. The workspace opens
//! what's chosen (`ResultsEvent::Open`) and draws the island around it.

use std::{ops::Range, path::PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, Render, ScrollStrategy, SharedString, StyledText,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    lsp::protocol::{Encoding, Location, Position},
    theme::Theme,
    ui::{HoverFill, icon_el},
};

/// A place to open: a 0-based line and a byte column in it.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultTarget {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

/// What a row shows before its label.
#[derive(Clone, Debug, PartialEq)]
pub enum Leading {
    /// A file: its icon (the label is its name).
    File(PathBuf),
    /// A line in a file, 1-based.
    Line(u32),
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
    pub expanded: bool,
}

/// A row on screen: a node by its index path from the tab's roots.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    path: Vec<usize>,
    depth: usize,
}

struct ResultTab {
    title: SharedString,
    /// Leaves with a place, shown next to the title.
    count: usize,
    nodes: Vec<ResultNode>,
    /// The expanded nodes, depth first.
    rows: Vec<Row>,
    selected: Option<usize>,
    pinned: bool,
}

impl ResultTab {
    fn new(title: SharedString, nodes: Vec<ResultNode>) -> Self {
        fn leaves(nodes: &[ResultNode]) -> usize {
            nodes.iter().map(|n| if n.children.is_empty() { n.target.is_some() as usize } else { leaves(&n.children) }).sum()
        }
        let mut tab = ResultTab { title, count: leaves(&nodes), nodes, rows: Vec::new(), selected: None, pinned: false };
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

    fn set_expanded(&mut self, row: usize, expanded: bool) {
        let path = self.rows[row].path.clone();
        let node = self.node_mut(&path);
        if node.children.is_empty() || node.expanded == expanded {
            return;
        }
        node.expanded = expanded;
        self.relayout();
    }

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
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl ResultsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { tabs: Vec::new(), active: 0, focus: cx.focus_handle(), scroll: UniformListScrollHandle::new() }
    }

    /// Shows a search's results: in place of the current tab unless that's
    /// pinned.
    pub fn show(&mut self, title: SharedString, nodes: Vec<ResultNode>, cx: &mut Context<Self>) {
        let tab = ResultTab::new(title, nodes);
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
        if node.children.is_empty() {
            return;
        }
        if node.expanded {
            tab.selected = Some(row + 1);
        } else {
            tab.set_expanded(row, true);
        }
        self.reveal_selected();
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab() else { return };
        let Some(row) = tab.selected else { return };
        let path = tab.rows[row].path.clone();
        let node = tab.node(&path);
        if node.target.is_none() && !node.children.is_empty() {
            let expanded = node.expanded;
            tab.set_expanded(row, !expanded);
            cx.notify();
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
                .child(div().text_color(theme.text_dim).child(tab.count.to_string()))
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
        let chevron = if node.children.is_empty() {
            div().size(px(14.)).flex_shrink_0().into_any_element()
        } else {
            icon_el(if node.expanded { Icon::ChevronDown } else { Icon::ChevronRight }, px(14.), theme.text_dim.into()).into_any_element()
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
                let Some(tab) = this.tab() else { return };
                let node = tab.node(&tab.rows[ix].path.clone()).clone();
                if node.target.is_none() || event.click_count() > 1 {
                    tab.set_expanded(ix, !node.expanded);
                }
                this.select(ix, Some(node.target.is_some()), cx);
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
            target: Some(ResultTarget { path: location.path.clone(), line: location.range.start.line, column: start as u32 }),
            children: Vec::new(),
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
        assert_eq!(second.target, Some(ResultTarget { path: "/p/src/b.rs".into(), line: 9, column: 8 }));
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
    fn stepping_goes_through_leaves_and_opens_groups() {
        let lines = vec!["a".into(), "a".into(), "a".into()];
        let mut nodes = usages_tree(vec![at("/x", 0, 0, 1), at("/x", 1, 0, 1), at("/y", 0, 0, 1)], lines, Encoding::Utf8, |p| p.display().to_string());
        nodes[1].expanded = false;
        let mut tab = ResultTab::new("t".into(), nodes);
        assert_eq!(tab.count, 3);
        assert_eq!(tab.rows.len(), 4);
        let lines_of = |t: Option<ResultTarget>| t.map(|t| (t.path, t.line));
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
