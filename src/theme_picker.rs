//! Theme switcher (JetBrains' "Quick Switch Scheme"). Moving the selection
//! previews the theme live; Enter keeps it and Escape restores the original.

use std::{ops::Range, sync::Arc};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    Render, Rgba, ScrollStrategy, Subscription, UniformListScrollHandle, Window, div, prelude::*,
    px, uniform_list,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline},
    theme::{self, Appearance, Theme, ThemeRegistry},
    ui::{HoverFill, icon_el, input_box, modal_panel, text_button, tooltip_view},
};

pub enum ThemePickerEvent {
    Confirmed(String),
    Dismissed,
}

#[derive(Clone)]
struct Row {
    theme: Arc<Theme>,
    custom: bool,
}

pub struct ThemePicker {
    query: Entity<Editor>,
    /// The theme to restore if the picker is dismissed.
    pub original: String,
    pub confirmed: bool,
    rows: Arc<Vec<Row>>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ThemePickerEvent> for ThemePicker {}

impl ThemePicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Pick up themes added or edited since the last scan.
        theme::reload_themes(cx);
        let query = cx.new(|cx| Editor::single_line("Select theme", cx));
        window.focus(&query.focus_handle(cx));
        let subscription = cx.subscribe(&query, |this, _, _: &EditorEvent, cx| this.update_rows(cx));
        let mut this = Self {
            query,
            original: Theme::get(cx).name.to_string(),
            confirmed: false,
            rows: Arc::new(Vec::new()),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![subscription],
        };
        this.update_rows(cx);
        let current = this.original.clone();
        if let Some(ix) = this.rows.iter().position(|r| r.theme.name.as_ref() == current) {
            this.selected = ix;
            this.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        this
    }

    fn update_rows(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let registry = cx.global::<ThemeRegistry>();
        let mut rows: Vec<Row> = registry
            .entries
            .iter()
            .filter(|e| name_matches(&e.theme.name, &query))
            .map(|e| Row {
                theme: e.theme.clone(),
                custom: e.path.is_some(),
            })
            .collect();
        // Dark themes first, then light; each in registry order.
        rows.sort_by_key(|r| r.theme.appearance == Appearance::Light);
        self.rows = Arc::new(rows);
        self.select(0, cx);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            cx.notify();
            return;
        }
        self.selected = ix.min(self.rows.len() - 1);
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        let name = self.rows[self.selected].theme.name.to_string();
        theme::apply_theme(&name, cx);
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(self.selected) {
            self.confirmed = true;
            cx.emit(ThemePickerEvent::Confirmed(row.theme.name.to_string()));
        }
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ThemePickerEvent::Dismissed);
    }

    fn open_folder(&mut self, cx: &mut Context<Self>) {
        match theme::ensure_themes_dir() {
            Ok(dir) => {
                cx.open_url(&format!("file://{}", dir.display()));
                self.reload(cx);
            }
            Err(e) => eprintln!("couldn't create themes folder: {e}"),
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let keep = self.rows.get(self.selected).map(|r| r.theme.name.to_string());
        theme::reload_themes(cx);
        self.update_rows(cx);
        if let Some(ix) = keep.and_then(|k| self.rows.iter().position(|r| r.theme.name.as_ref() == k)) {
            self.select(ix, cx);
        }
    }
}

impl Focusable for ThemePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

/// Whether `name` contains `query`, ignoring case and accents ("rose" finds
/// Rosé Pine).
fn name_matches(name: &str, query: &str) -> bool {
    use nucleo_matcher::{
        Config, Matcher, Utf32Str,
        pattern::{Atom, AtomKind, CaseMatching, Normalization},
    };
    if query.trim().is_empty() {
        return true;
    }
    let atom = Atom::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Substring, false);
    let mut buf = Vec::new();
    atom.score(Utf32Str::new(name, &mut buf), &mut Matcher::new(Config::DEFAULT)).is_some()
}

fn swatch(color: Rgba) -> impl IntoElement {
    div().size(px(12.)).rounded_full().bg(color)
}

impl Render for ThemePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let rows = self.rows.clone();
        let selected = self.selected;
        let errors = cx.global::<ThemeRegistry>().errors.clone();
        let dir = theme::themes_dir().map(|d| theme::display_path(&d)).unwrap_or_default();

        let list = uniform_list("themes", rows.len(), {
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let current = Theme::get(cx).clone();
                range
                    .map(|ix| {
                        let row = &rows[ix];
                        let t = &row.theme;
                        let is_selected = ix == selected;
                        div().w_full().px_1().child(
                            div()
                                .id(ix)
                                .relative()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(px(32.))
                                .px_2p5()
                                .rounded(px(8.))
                                .cursor_pointer()
                                .when(is_selected, |el| el.bg(current.accent_soft))
                                .row_hover(current.hover)
                                .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                                    this.select(ix, cx);
                                    if e.click_count() >= 2 {
                                        this.confirm(cx);
                                    }
                                }))
                                // A mini preview: the editor background with accent and syntax dots.
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .px_1p5()
                                        .py_1()
                                        .rounded_full()
                                        .bg(t.background)
                                        .border_1()
                                        .border_color(current.border)
                                        .child(swatch(t.accent))
                                        .child(swatch(t.syntax.keyword))
                                        .child(swatch(t.syntax.string))
                                        .child(swatch(t.syntax.function)),
                                )
                                .child(div().flex_1().text_color(current.text).child(t.name.clone()))
                                .when(row.custom, |el| {
                                    el.child(
                                        div()
                                            .px_1p5()
                                            .rounded(px(4.))
                                            .bg(current.accent_soft)
                                            .text_size(px(11.))
                                            .text_color(current.accent)
                                            .child("Custom"),
                                    )
                                })
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_size(px(12.))
                                        .text_color(current.text_dim)
                                        .child(match t.appearance {
                                            Appearance::Dark => "Dark",
                                            Appearance::Light => "Light",
                                        }),
                                ),
                        )
                    })
                    .collect()
            })
        })
        .track_scroll(self.scroll.clone())
        .h(px((self.rows.len().clamp(1, 12) * 32) as f32))
        .w_full();
        // Padding goes on a wrapper: GPUI's scroll-to-item counts a list's own
        // padding twice, so a list that fits its rows would scroll by 8px.
        let list = div().py_1().child(div().relative().child(list).child(crate::ui::list_scrollbar("themes-scrollbar", &self.scroll, cx)));

        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pb_2p5()
            .pt_0p5()
            .text_size(px(12.))
            .text_color(theme.text_dim)
            .child(
                div()
                    .id("theme-status")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(if errors.is_empty() {
                        format!("Add your own .json themes to {dir}")
                    } else {
                        format!("{} theme problem(s): {}", errors.len(), errors[0])
                    })
                    .when(!errors.is_empty(), |el| {
                        let all = errors.join("\n");
                        el.text_color(theme.warning)
                            .tooltip(move |_, cx| tooltip_view(all.clone(), cx))
                    }),
            )
            .child(text_button("theme-reload", "Reload", false, cx).on_click(cx.listener(|this, _, _, cx| this.reload(cx))))
            .child(text_button("theme-folder", "Open Themes Folder", true, cx).on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))));

        modal_panel(cx)
            .key_context("ThemePicker")
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .w(px(520.))
            .max_w(window.viewport_size().width - px(48.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_3p5()
                    .pt_3()
                    .pb_2()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::Palette, px(13.), theme.text_muted.into()))
                    .child("Select Theme"),
            )
            .child(div().px_2p5().pb_2().child(input_box(&self.query, Some(Icon::Search), window, cx)))
            .child(crate::ui::island(&theme).p_0().mx(px(crate::ui::GAP)).mb(px(crate::ui::GAP)).child(list))
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::name_matches;

    #[test]
    fn theme_names_match_without_case_or_accents() {
        assert!(name_matches("Rosé Pine", "rose"));
        assert!(name_matches("Rosé Pine", "ROSÉ p"));
        assert!(name_matches("Tokyo Night", ""));
        assert!(!name_matches("Tokyo Night", "dawn"));
    }
}
