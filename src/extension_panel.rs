//! Extensions' panels in the bottom area (`[kuana.panels.<id>]`): what their
//! code shows there (`show-panel`), lines of text or a table, one tab per
//! panel. Showing a panel again replaces its content.

use std::ops::Range;

use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, UniformListScrollHandle, Window,
    div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    extensions::ui_host::Content,
    grid::GridTable,
    theme::Theme,
    ui::{HoverFill, tooltip_view},
};

enum Body {
    Text(Vec<SharedString>),
    Table(Entity<GridTable>),
}

struct Tab {
    extension: String,
    panel: String,
    /// The panel's title (its manifest's).
    title: String,
    /// This content's own title, from the code.
    subtitle: Option<String>,
    body: Body,
}

pub enum ExtensionPanelsEvent {
    Close,
}

pub struct ExtensionPanels {
    focus: FocusHandle,
    tabs: Vec<Tab>,
    active: usize,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<ExtensionPanelsEvent> for ExtensionPanels {}

impl ExtensionPanels {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { focus: cx.focus_handle(), tabs: Vec::new(), active: 0, scroll: UniformListScrollHandle::new() }
    }

    /// Shows `content` in an extension's panel, its tab made current.
    pub fn show(&mut self, extension: String, panel: String, title: String, subtitle: Option<String>, content: Content, cx: &mut Context<Self>) {
        let body = match content {
            Content::Text(lines) => Body::Text(lines.into_iter().map(SharedString::from).collect()),
            Content::Table { columns, rows } => {
                let numeric = (0..columns.len())
                    .map(|c| {
                        let values: Vec<&str> = rows.iter().filter_map(|r| r.get(c).and_then(|v| v.as_deref())).collect();
                        !values.is_empty() && values.iter().all(|v| v.parse::<f64>().is_ok())
                    })
                    .collect();
                Body::Table(cx.new(|cx| GridTable::new(columns, Vec::new(), numeric, rows, cx)))
            }
        };
        let tab = Tab { extension, panel, title, subtitle, body };
        match self.tabs.iter().position(|t| t.extension == tab.extension && t.panel == tab.panel) {
            Some(ix) => {
                self.tabs[ix] = tab;
                self.active = ix;
            }
            None => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
        }
        self.scroll = UniformListScrollHandle::new();
        cx.notify();
    }

    /// An extension went: its tabs go too.
    pub fn retain_extensions(&mut self, keep: impl Fn(&str) -> bool, cx: &mut Context<Self>) {
        let before = self.tabs.len();
        self.tabs.retain(|t| keep(&t.extension));
        if self.tabs.len() != before {
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            cx.notify();
        }
    }

    fn copy(&self, cx: &mut App) {
        match self.tabs.get(self.active).map(|t| &t.body) {
            Some(Body::Text(lines)) => cx.write_to_clipboard(ClipboardItem::new_string(lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>().join("\n"))),
            Some(Body::Table(table)) => GridTable::copy_all(&table.clone(), cx),
            None => {}
        }
    }
}

impl Focusable for ExtensionPanels {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ExtensionPanels {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let tabs = div().flex().items_center().gap_0p5().min_w_0().overflow_hidden().children(self.tabs.iter().enumerate().map(|(ix, tab)| {
            let on = ix == self.active;
            div()
                .id(("ext-panel-tab", ix))
                .flex()
                .flex_shrink_0()
                .items_center()
                .h(px(24.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on { theme.text } else { theme.text_muted })
                .child(tab.title.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = ix;
                    this.scroll = UniformListScrollHandle::new();
                    cx.notify();
                }))
                .hover_fill(if on { gpui::Hsla::from(theme.active) } else { gpui::Hsla::from(theme.hover).alpha(0.) }, if on { theme.active } else { theme.hover })
        }));
        let subtitle = self.tabs.get(self.active).and_then(|t| t.subtitle.clone());
        let header = crate::ui::panel_header()
            .child(tabs)
            .children(subtitle.map(|s| div().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_dim).child(s)))
            .child(div().flex_1())
            .when(!self.tabs.is_empty(), |el| {
                el.child(
                    crate::ui::icon_button("ext-panel-copy", Icon::Copy, theme.text_dim.into(), cx)
                        .tooltip(|_, cx| tooltip_view("Copy All", cx))
                        .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                )
            })
            .child(crate::ui::panel_close_button("close-ext-panel", cx).on_click(cx.listener(|_, _, _, cx| cx.emit(ExtensionPanelsEvent::Close))));
        let body = match self.tabs.get(self.active).map(|t| &t.body) {
            None => div().p_3().text_color(theme.text_dim).child("Extensions show their output here.").into_any_element(),
            Some(Body::Table(table)) => div().flex_1().min_h_0().child(table.clone()).into_any_element(),
            Some(Body::Text(lines)) => {
                let count = lines.len();
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "ext-panel-lines",
                            count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                let theme = Theme::get(cx);
                                let Some(Body::Text(lines)) = this.tabs.get(this.active).map(|t| &t.body) else { return Vec::new() };
                                range
                                    .filter(|&i| i < lines.len())
                                    .map(|i| {
                                        div()
                                            .id(("ext-line", i))
                                            .w_full()
                                            .h(px(20.))
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .font_family(theme.mono_font.clone())
                                            .text_size(px(12.))
                                            .text_color(theme.text)
                                            .child(div().truncate().child(lines[i].clone()))
                                            .into_any_element()
                                    })
                                    .collect()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full()
                        .py_1(),
                    )
                    .child(crate::ui::list_scrollbar("ext-panel-scrollbar", &self.scroll, cx))
                    .into_any_element()
            }
        };
        div()
            .key_context("ExtensionPanels")
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
    }
}
