//! A view an extension adds to the activity bar (`[kuana.views.<id>]`): a
//! tree Kuana draws from items the extension's code supplies
//! (`extensions::ui_host::children`), loaded as nodes open. The header holds
//! the view's toolbar commands; a click runs an item's command (or opens
//! it), a right-click asks the workspace for its menu. `refresh` (from the
//! code, or Refresh) reads the open nodes again, coalesced so a chatty
//! extension can't redraw in a loop.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, Render, ScrollStrategy, SharedString,
    Task, UniformListScrollHandle, Window, div, prelude::*, px, svg, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    extensions::{
        contrib::{Contributions, FILE_ICON_PREFIX, MenuPlace, ViewDef},
        ui_host::{self, Caller, Tone, TreeItem},
    },
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

/// How long refresh requests gather before one reload.
const REFRESH_COALESCE: Duration = Duration::from_millis(100);

enum Loaded {
    Loading,
    Ready(Vec<TreeItem>),
    Failed(String),
}

struct Row {
    /// None for a "Loading…" or error row.
    item: Option<TreeItem>,
    /// The node it's under (for a message row) or its own id.
    key: String,
    depth: usize,
    message: Option<(String, bool)>,
    expanded: bool,
}

pub enum ExtensionViewEvent {
    /// Run one of the extension's commands (on an item, when given).
    Run { command: String, target: Option<String> },
    /// An item's right-click menu.
    Menu { item: TreeItem, position: gpui::Point<gpui::Pixels> },
    /// Escape: back to the editor.
    Leave,
}

pub struct ExtensionView {
    extension: Arc<Contributions>,
    view: ViewDef,
    caller: Caller,
    focus: FocusHandle,
    /// Loaded children by parent (None: the roots).
    children: HashMap<Option<String>, Loaded>,
    expanded: HashSet<String>,
    selected: Option<String>,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    /// Nodes to read again once the coalescing pause is over (None: all).
    pending: Option<Option<HashSet<String>>>,
    _refresh: Option<Task<()>>,
}

impl EventEmitter<ExtensionViewEvent> for ExtensionView {}

impl ExtensionView {
    pub fn new(extension: Arc<Contributions>, view: ViewDef, caller: Caller, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            extension,
            view,
            caller,
            focus: cx.focus_handle(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            pending: None,
            _refresh: None,
        };
        this.load(None, cx);
        this
    }

    pub fn extension(&self) -> &Arc<Contributions> {
        &self.extension
    }

    pub fn view(&self) -> &ViewDef {
        &self.view
    }

    /// The extension changed (reloaded): its new definition, everything read again.
    pub fn set_extension(&mut self, extension: Arc<Contributions>, view: ViewDef, cx: &mut Context<Self>) {
        self.extension = extension;
        self.view = view;
        self.refresh(None, cx);
    }

    /// The project (or settings) the code sees changed.
    pub fn set_caller(&mut self, caller: Caller, cx: &mut Context<Self>) {
        let moved = caller.root != self.caller.root;
        self.caller = caller;
        if moved {
            self.refresh(None, cx);
        }
    }

    /// Reads `item`'s children again (None: the whole view), after a short
    /// pause that gathers other requests.
    pub fn refresh(&mut self, item: Option<String>, cx: &mut Context<Self>) {
        self.pending = match (self.pending.take(), item) {
            (Some(None), _) | (_, None) => Some(None),
            (Some(Some(mut items)), Some(item)) => {
                items.insert(item);
                Some(Some(items))
            }
            (None, Some(item)) => Some(Some(HashSet::from([item]))),
        };
        self._refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_COALESCE).await;
            this.update(cx, |this, cx| {
                match this.pending.take() {
                    Some(None) | None => {
                        // Keep the open nodes; read the roots and those again.
                        let open: Vec<String> = this.expanded.iter().cloned().collect();
                        this.children.clear();
                        this.load(None, cx);
                        for node in open {
                            this.load(Some(node), cx);
                        }
                    }
                    Some(Some(items)) => {
                        for item in items {
                            if this.children.remove(&Some(item.clone())).is_some() || this.expanded.contains(&item) {
                                this.load(Some(item), cx);
                            }
                        }
                    }
                }
            })
            .ok();
        }));
    }

    fn load(&mut self, parent: Option<String>, cx: &mut Context<Self>) {
        self.children.insert(parent.clone(), Loaded::Loading);
        self.rebuild();
        cx.notify();
        let job = ui_host::children(self.extension.clone(), self.caller.clone(), self.view.id.clone(), parent.clone());
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                // A refresh since replaced this request.
                if !matches!(this.children.get(&parent), Some(Loaded::Loading)) {
                    return;
                }
                this.children.insert(parent, match result {
                    Ok(items) => Loaded::Ready(items),
                    Err(err) => Loaded::Failed(err),
                });
                this.rebuild();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        self.push_rows(None, 0, &mut rows);
        self.rows = rows;
    }

    fn push_rows(&self, parent: Option<&String>, depth: usize, rows: &mut Vec<Row>) {
        let key = parent.cloned().unwrap_or_default();
        let message = |text: String, error: bool| Row { item: None, key: key.clone(), depth, message: Some((text, error)), expanded: false };
        match self.children.get(&parent.cloned()) {
            None | Some(Loaded::Loading) => rows.push(message("Loading…".into(), false)),
            Some(Loaded::Failed(err)) => rows.push(message(err.clone(), true)),
            Some(Loaded::Ready(items)) => {
                for item in items {
                    let expanded = item.expandable && self.expanded.contains(&item.id);
                    rows.push(Row { item: Some(item.clone()), key: item.id.clone(), depth, message: None, expanded });
                    if expanded {
                        self.push_rows(Some(&item.id), depth + 1, rows);
                    }
                }
            }
        }
    }

    fn set_expanded(&mut self, id: &str, open: bool, cx: &mut Context<Self>) {
        if open {
            self.expanded.insert(id.to_string());
            if !self.children.contains_key(&Some(id.to_string())) {
                self.load(Some(id.to_string()), cx);
                return;
            }
        } else {
            self.expanded.remove(id);
        }
        self.rebuild();
        cx.notify();
    }

    /// A row's click: its command, else open or close it.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(item) = self.rows.get(ix).and_then(|r| r.item.clone()) else { return };
        let expanded = self.rows[ix].expanded;
        self.selected = Some(item.id.clone());
        match &item.command {
            Some(command) => cx.emit(ExtensionViewEvent::Run { command: command.clone(), target: Some(item.id.clone()) }),
            None if item.expandable => self.set_expanded(&item.id, !expanded, cx),
            None => {}
        }
        cx.notify();
    }

    fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.rows.get(ix).and_then(|r| r.item.as_ref()) {
            self.selected = Some(item.id.clone());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows.iter().position(|r| r.item.as_ref().is_some_and(|i| i.id == *selected))
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let items: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].item.is_some()).collect();
        let at = self.selected_index().and_then(|s| items.iter().position(|&i| i == s));
        let next = match (at, forward) {
            (None, _) => items.first(),
            (Some(p), true) => items.get(p + 1).or(items.last()),
            (Some(p), false) => items.get(p.saturating_sub(1)),
        };
        if let Some(&ix) = next {
            self.select_index(ix, cx);
        }
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn on_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else { return };
        let (expanded, depth, id) = (self.rows[ix].expanded, self.rows[ix].depth, self.rows[ix].key.clone());
        if expanded {
            self.set_expanded(&id, false, cx);
        } else if let Some(parent) = (0..ix).rev().find(|&i| self.rows[i].depth < depth) {
            self.select_index(parent, cx);
        }
    }

    fn on_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else { return };
        if let Some(item) = self.rows[ix].item.clone().filter(|i| i.expandable && !self.rows[ix].expanded) {
            self.set_expanded(&item.id, true, cx);
        }
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected_index() {
            Some(ix) => self.activate(ix, cx),
            None => self.step(true, cx),
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ExtensionViewEvent::Leave);
    }

    /// An item's icon as an asset path: one of Kuana's, or an SVG in the
    /// extension's folder (nothing outside it).
    fn icon_path(&self, icon: &str) -> Option<SharedString> {
        if crate::assets::has_icon(icon) {
            return Some(format!("icons/{icon}.svg").into());
        }
        let path = crate::extensions::wasm::inside(&self.extension.dir, icon).ok()?;
        (path.extension().is_some_and(|e| e == "svg") && path.is_file()).then(|| format!("{FILE_ICON_PREFIX}{}", path.display()).into())
    }

    fn render_row(&self, ix: usize, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::get(cx);
        let row = &self.rows[ix];
        let color = |tone: Tone| -> Hsla {
            match tone {
                Tone::Normal => theme.text.into(),
                Tone::Muted => theme.text_dim.into(),
                Tone::Success => theme.success.into(),
                Tone::Warning => theme.warning.into(),
                Tone::Error => theme.error.into(),
            }
        };
        let base = div().id(("ext-row", ix)).w_full().h(px(24.)).flex().items_center().gap_1p5().pl(px(6. + 14. * row.depth as f32)).pr_2();
        let Some(item) = row.item.clone() else {
            let (text, error) = row.message.clone().unwrap_or_default();
            return base
                .child(div().size(px(14.)).flex_shrink_0())
                .child(div().min_w_0().truncate().text_size(px(12.)).text_color(if error { theme.error } else { theme.text_dim }).child(text))
                .into_any_element();
        };
        let selected = self.selected.as_deref() == Some(item.id.as_str());
        let chevron = if item.expandable {
            let (id, expanded) = (item.id.clone(), row.expanded);
            div()
                .id(("ext-chevron", ix))
                .flex_shrink_0()
                .child(icon_el(if expanded { Icon::ChevronDown } else { Icon::ChevronRight }, px(14.), theme.text_dim.into()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus);
                    this.selected = Some(id.clone());
                    this.set_expanded(&id, !expanded, cx);
                }))
                .into_any_element()
        } else {
            div().size(px(14.)).flex_shrink_0().into_any_element()
        };
        let icon = item.icon.as_deref().and_then(|i| self.icon_path(i));
        let tone_color = color(item.tone);
        let label_color = if matches!(item.tone, Tone::Muted) { theme.text_muted.into() } else { Hsla::from(theme.text) };
        let selected_bg = if focused { theme.accent_soft } else { theme.active };
        let menu_item = item.clone();
        base.cursor_pointer()
            .when(selected, |el| el.bg(selected_bg))
            .row_hover(theme.hover)
            .child(chevron)
            .children(icon.map(|path| svg().path(path).size(px(14.)).flex_shrink_0().text_color(tone_color)))
            .child(div().min_w_0().truncate().text_color(label_color).child(item.label.clone()))
            .children(item.detail.clone().map(|d| div().flex_shrink().min_w_0().truncate().text_size(px(11.5)).text_color(theme.text_dim).child(d)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus);
                // A double click opens or closes an item that has a command too.
                if event.click_count() > 1 {
                    if let Some(row) = this.rows.get(ix).filter(|r| r.item.as_ref().is_some_and(|i| i.expandable)) {
                        let (id, expanded) = (row.key.clone(), row.expanded);
                        this.set_expanded(&id, !expanded, cx);
                    }
                    return;
                }
                this.activate(ix, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    this.selected = Some(menu_item.id.clone());
                    cx.emit(ExtensionViewEvent::Menu { item: menu_item.clone(), position: event.position });
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}

impl Focusable for ExtensionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ExtensionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let focused = self.focus.contains_focused(window, cx);
        let place = MenuPlace::ViewToolbar(self.view.id.clone());
        let tools: Vec<(String, String, SharedString)> = self
            .extension
            .menu(&place, |_| true)
            .map(|c| (c.id.clone(), c.title.clone(), c.icon.clone().unwrap_or_else(|| "icons/more.svg".into())))
            .collect();
        let header = crate::ui::view_header()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_1p5()
                    .truncate()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(self.view.title.to_uppercase()),
            )
            .children(tools.into_iter().enumerate().map(|(i, (command, title, icon))| {
                div()
                    .id(("ext-tool", i))
                    .size(px(26.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .child(svg().path(icon).size(px(15.)).flex_shrink_0().text_color(theme.text_muted))
                    .tooltip(move |_, cx| tooltip_view(title.clone(), cx))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ExtensionViewEvent::Run { command: command.clone(), target: None })))
                    .hover_bg(theme.hover)
            }));
        let rows = self.rows.len();
        div()
            .key_context("ExtensionView")
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
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "ext-tree",
                            rows,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                range.filter(|&ix| ix < this.rows.len()).map(|ix| this.render_row(ix, focused, cx)).collect()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full()
                        .pb_2(),
                    )
                    .child(crate::ui::list_scrollbar("ext-scrollbar", &self.scroll, cx)),
            )
    }
}
