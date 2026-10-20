//! Welcome Setup: the first thing a fresh install shows (and Find Action's
//! "Welcome Setup" later). It picks a theme, a base keymap and a few editor
//! basics. Like Preferences, every choice is written to `settings.json` and
//! applied right away, so the page itself restyles as themes are tried.

use gpui::{
    AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    KeyDownEvent, Render, Rgba, SharedString, Window, div, img, prelude::*, px,
};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};

use crate::{
    assets::{self, Icon},
    keymap,
    settings::{self, AutoSave, BaseKeymap, Settings, VisualEffects},
    theme::{self, Appearance, Theme, ThemeRegistry},
    transition::{self, TransitionExt, mix},
    ui::{Button, HoverFill, faded, field_glow, icon_el, kbd, switch},
    workspace::{FindAction, GoToFile, OpenSettings},
};
use crate::editor::{DeleteLine, DuplicateLine};

pub enum OnboardingEvent {
    /// Get Started (or Escape / Enter).
    Done,
    /// All Settings…: finish and open Preferences.
    OpenSettings,
    /// Browse All Themes…: the theme picker.
    BrowseThemes,
    Problems(Vec<String>),
}

/// The themes offered first; the rest are a click away in the theme picker.
const FEATURED_THEMES: [&str; 8] =
    ["Ember", "Clay Black", "Lighthouse Night", "Dracula", "Sunset Fig", "Campfire", "One Light", "Camellia Light"];

pub struct Onboarding {
    focus_handle: FocusHandle,
    scroll: gpui::ScrollHandle,
}

impl EventEmitter<OnboardingEvent> for Onboarding {}

impl Onboarding {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self { focus_handle: cx.focus_handle(), scroll: gpui::ScrollHandle::new() }
    }

    /// Applies what was just written, reporting anything that went wrong.
    fn write(&mut self, result: std::io::Result<()>, cx: &mut Context<Self>) {
        let mut problems: Vec<String> = result.err().map(|e| format!("settings.json: {e}")).into_iter().collect();
        problems.extend(settings::reload_now(cx));
        if !problems.is_empty() {
            cx.emit(OnboardingEvent::Problems(problems));
        }
        cx.notify();
    }

    /// Sets a key, or removes it when `value` is the default, so the file only
    /// lists what was changed.
    fn set_or_default(&mut self, key: &str, value: Value, default: Value, cx: &mut Context<Self>) {
        let result = if value == default { Settings::remove(key) } else { Settings::set(key, value) };
        self.write(result, cx);
    }

    fn choose_theme(&mut self, name: SharedString, cx: &mut Context<Self>) {
        theme::apply_theme(&name, cx);
        self.set_or_default("theme", json!(name.as_ref()), json!(theme::DEFAULT_THEME), cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        if k.modifiers.number_of_modifiers() == 0 && matches!(k.key.as_str(), "escape" | "enter") {
            cx.stop_propagation();
            cx.emit(OnboardingEvent::Done);
        }
    }

    // ---- Sections -----------------------------------------------------------------------

    fn render_themes(&self, columns: usize, cx: &mut Context<Self>) -> AnyElement {
        let current = Theme::get(cx).clone();
        let registry = cx.global::<ThemeRegistry>();
        let mut themes: Vec<_> = FEATURED_THEMES.iter().filter_map(|name| registry.find(name)).map(|e| e.theme.clone()).collect();
        // Whatever is in use stays visible, even when it isn't one of the featured.
        if !themes.iter().any(|t| t.name == current.name)
            && let Some(entry) = registry.find(&current.name)
        {
            themes.pop();
            themes.insert(0, entry.theme.clone());
        }
        let total = registry.entries.len();
        let cards: Vec<AnyElement> = themes
            .into_iter()
            .enumerate()
            .map(|(ix, t)| {
                let selected = t.name == current.name;
                let name = t.name.clone();
                let card = div()
                    .id(("welcome-theme", ix))
                    .flex()
                    .flex_col()
                    .rounded(px(12.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_theme(name.clone(), cx)))
                    .child(code_preview(&t))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .h(px(32.))
                            .px_3()
                            // Clipping doesn't follow rounded corners, so the
                            // halves round their own (inside the 1px border).
                            .rounded_b(px(11.))
                            .bg(t.panel)
                            .border_t_1()
                            .border_color(t.border)
                            .text_size(px(12.5))
                            .text_color(t.text)
                            .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(t.text_dim)
                                    .child(if t.appearance == Appearance::Light { "Light" } else { "Dark" }),
                            )
                            .when(selected, |el| el.child(icon_el(Icon::CircleCheck, px(14.), t.accent.into()))),
                    );
                choice_card(card, format!("welcome-theme-{ix}"), selected, &current).into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(grid(cards, columns, px(12.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(12.)).text_color(current.text_dim).child(format!("{total} themes built in, and you can add your own.")))
                    .child(
                        link_button("welcome-all-themes", "Browse All Themes", &current)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(OnboardingEvent::BrowseThemes))),
                    ),
            )
            .child(option_rows(
                vec![(
                    "Reduce visual effects",
                    "Solid colors, and no shadows, glows or animations. Settings has a level in between.",
                    // On is the Minimal level (Settings → Appearance has Reduced too).
                    self.toggle(
                        "welcome-reduce-effects",
                        Settings::get(cx).visual_effects() == VisualEffects::Minimal,
                        &current,
                        cx,
                        |this, on, cx| {
                            let level = if on { VisualEffects::Minimal } else { VisualEffects::Full };
                            this.write(Settings::set_visual_effects(level), cx);
                        },
                    ),
                )],
                &current,
            ))
            .into_any_element()
    }

    fn render_keymaps(&self, columns: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let current = keymap::current(cx);
        let samples: [(&str, &dyn gpui::Action); 4] =
            [("Go to File", &GoToFile), ("Find Action", &FindAction), ("Duplicate Line", &DuplicateLine), ("Delete Line", &DeleteLine)];
        let cards: Vec<AnyElement> = BaseKeymap::ALL
            .iter()
            .enumerate()
            .map(|(ix, &base)| {
                let selected = base == current;
                let rows = samples.iter().map(|(label, action)| {
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .h(px(22.))
                        .child(div().min_w_0().truncate().text_size(px(12.)).text_color(theme.text_muted).child(*label))
                        .child(kbd(keymap::shortcut_in(base, *action).unwrap_or_else(|| "—".into()), cx))
                });
                let card = div()
                    .id(("welcome-keymap", ix))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(14.))
                    .rounded(px(12.))
                    .bg(theme.panel)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_or_default("base_keymap", base.key().into(), BaseKeymap::default().key().into(), cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(base.label()))
                            .child(
                                div()
                                    .size(px(16.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(if selected { Hsla::from(theme.accent) } else { Hsla::from(theme.border) })
                                    .when(selected, |el| el.child(div().size(px(8.)).rounded_full().bg(theme.accent))),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if selected { theme.text_muted } else { theme.text_dim })
                            .child(keymap_blurb(base)),
                    )
                    .child(div().h(px(1.)).bg(theme.border_subtle))
                    .children(rows);
                choice_card(card, format!("welcome-keymap-{ix}"), selected, &theme).into_any_element()
            })
            .collect();
        grid(cards, columns, px(12.)).into_any_element()
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let settings = Settings::get(cx).clone();
        let size = settings.editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
        let auto_save = settings.auto_save.unwrap_or_default() != AutoSave::Off;

        let step = |id: &'static str, icon: Icon, delta: f32, enabled: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .when(enabled, |el| {
                    el.cursor_pointer().on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let size = Settings::get(cx).editor_font_size.unwrap_or(theme::DEFAULT_EDITOR_FONT_SIZE);
                        let size = (size.round() + delta).clamp(8., 32.);
                        this.set_or_default("editor_font_size", json!(size), json!(theme::DEFAULT_EDITOR_FONT_SIZE), cx);
                        // What's picked here is what shows, without an old zoom on top.
                        theme::zoom(None, cx);
                    }))
                })
                .hover_bg(if enabled { Hsla::from(theme.hover) } else { gpui::transparent_black() })
                .child(icon_el(icon, px(13.), if enabled { theme.text_muted.into() } else { Hsla::from(theme.text_dim).alpha(0.5) }))
        };
        let font_size = div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .font_family(theme.mono_font.clone())
                    .text_size(px(size))
                    .line_height(px(size * 1.4))
                    .text_color(theme.syntax.keyword)
                    .child("Aa"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .p(px(2.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(step("welcome-font-down", Icon::Minus, -1., size > 8., cx))
                    .child(
                        div()
                            .min_w(px(44.))
                            .flex()
                            .justify_center()
                            .font_family(theme.mono_font.clone())
                            .text_size(px(12.5))
                            .child(format!("{size}")),
                    )
                    .child(step("welcome-font-up", Icon::Plus, 1., size < 32., cx)),
            );

        let toggle = |id: &'static str, on: bool, cx: &mut Context<Self>, set: fn(&mut Self, bool, &mut Context<Self>)| {
            self.toggle(id, on, &theme, cx, set)
        };
        let rows: Vec<(&str, &str, AnyElement)> = vec![
            ("Font size", "For code and the terminal. Zoom in and out any time.", font_size.into_any_element()),
            (
                "Save automatically",
                "A second after you stop typing, and when Kuana quits.",
                toggle("welcome-auto-save", auto_save, cx, |this, on, cx| {
                    let mode = if on { AutoSave::AfterDelay } else { AutoSave::Off };
                    this.set_or_default("auto_save", mode.key().into(), AutoSave::default().key().into(), cx);
                }),
            ),
            (
                "Reformat on save",
                "Run the project's formatter or language server when you save.",
                toggle("welcome-format", settings.editor.format_on_save.unwrap_or(false), cx, |this, on, cx| {
                    this.set_or_default("format_on_save", json!(on), json!(false), cx);
                }),
            ),
            (
                "Soft wrap",
                "Wrap long lines at the editor's edge. Markdown wraps either way.",
                toggle("welcome-soft-wrap", settings.editor.soft_wrap.unwrap_or(false), cx, |this, on, cx| {
                    this.set_or_default("soft_wrap", json!(on), json!(false), cx);
                }),
            ),
            (
                "Update automatically",
                "Download new versions in the background; you choose when to restart.",
                toggle("welcome-auto-update", settings.auto_update.unwrap_or(true), cx, |this, on, cx| {
                    this.set_or_default("auto_update", json!(on), json!(true), cx);
                }),
            ),
        ];
        option_rows(rows, &theme)
    }

    /// An on/off switch writing `set`'s choice to the settings.
    fn toggle(&self, id: &'static str, on: bool, theme: &Theme, cx: &mut Context<Self>, set: fn(&mut Self, bool, &mut Context<Self>)) -> AnyElement {
        div()
            .id(id)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| set(this, !on, cx)))
            .child(switch(on, theme))
            .into_any_element()
    }
}

impl Focusable for Onboarding {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Onboarding {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let width = window.viewport_size().width;
        let wide = width > px(860.);
        let settings_keys = crate::ui::binding_label(&OpenSettings, window);
        let header = div()
            .flex()
            .items_center()
            .gap_5()
            .child(img(assets::LOGO).size(px(72.)).rounded(px(18.)).flex_shrink_0())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_family(assets::TITLE_FONT)
                            .text_size(px(38.))
                            .line_height(px(44.))
                            .text_color(theme.text)
                            .child("Welcome to Kuana"),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme.text_muted)
                            .child("A few choices to make it feel like home. You can change all of them later in Settings."),
                    ),
            );
        let footer = div()
            .flex()
            .items_center()
            .gap_3()
            .pt_2()
            .child(
                // Wraps in a narrow window instead of pushing the buttons out
                // of view (GPUI measures text's minimum as one line, so it
                // shrinks only with `min_w_0`).
                div()
                    .flex_auto()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(div().min_w_0().child("Everything else is in Settings"))
                    .when_some(settings_keys, |el, keys| el.child(kbd(keys, cx))),
            )
            .child(
                link_button("welcome-settings", "All Settings…", &theme)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(OnboardingEvent::OpenSettings))),
            )
            .child(
                div()
                    .id("welcome-done")
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(36.))
                    .px_5()
                    // Text buttons are pills.
                    .rounded_full()
                    .bg(theme.accent)
                    .text_color(theme.background)
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.88))
                    .active(|s| s.opacity(0.75))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(OnboardingEvent::Done)))
                    .child("Get Started")
                    .child(icon_el(Icon::ArrowRight, px(14.), theme.background.into())),
            );

        let page = div()
            .id("onboarding")
            .key_context("Onboarding")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(theme.background)
            .child(
                div().flex().justify_center().px_6().py(px(44.)).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(36.))
                        .w_full()
                        .max_w(px(900.))
                        .child(header)
                        .child(section(1, "Choose a theme", "Applied as you click; the page follows along.", self.render_themes(if wide { 4 } else { 2 }, cx), &theme))
                        .child(section(
                            2,
                            "Pick your shortcuts",
                            "Start from the editor your hands already know. Your own changes always go on top.",
                            self.render_keymaps(if wide { 4 } else { 2 }, cx),
                            &theme,
                        ))
                        .child(section(3, "Editor basics", "The settings people change first.", self.render_editor(cx), &theme))
                        .child(div().h(px(1.)).bg(theme.border_subtle))
                        .child(footer),
                ),
            );
        div().relative().size_full().child(page).child(crate::ui::scrollbar("onboarding-scrollbar", &self.scroll, cx))
    }
}

/// A numbered step: title and a line of explanation over its content.
fn section(number: usize, title: &str, detail: &str, content: AnyElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(
            div()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .mt(px(1.))
                        .size(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.accent_soft)
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.accent)
                        .child(number.to_string()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(title.to_string()))
                        .child(div().text_size(px(13.)).text_color(theme.text_dim).child(detail.to_string())),
                ),
        )
        .child(content)
}

/// Rows of equal-width cells (GPUI's grid doesn't size these cards).
/// Settings as rows in a card: label and detail on the left, the control on the right.
fn option_rows(rows: Vec<(&str, &str, AnyElement)>, theme: &Theme) -> AnyElement {
    let count = rows.len();
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .border_1()
        .border_color(theme.border_subtle)
        .bg(theme.panel)
        .children(rows.into_iter().enumerate().map(|(ix, (label, detail, control))| {
            div()
                .flex()
                .items_center()
                .gap_4()
                .min_h(px(56.))
                .px_4()
                .py_2()
                .when(ix + 1 < count, |el| el.border_b_1().border_color(theme.border_subtle))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        // Measured at its own width, then shrunk: with a zero
                        // basis (`flex_1`) GPUI can wrap it a letter per line.
                        .flex_auto()
                        .min_w_0()
                        .child(div().text_color(theme.text).child(label.to_string()))
                        .child(div().text_size(px(12.)).text_color(theme.text_dim).child(detail.to_string())),
                )
                .child(div().flex_shrink_0().child(control))
        }))
        .into_any_element()
}

/// A card that stands for a choice. Like the Welcome tiles and focused text
/// fields, the outline and glow say what it is, not a fill: the chosen card's
/// border is the accent with a soft glow, and hovering another lights it part
/// of the way. Both fade, so a new choice hands the light over.
fn choice_card(card: gpui::Stateful<gpui::Div>, id: String, selected: bool, theme: &Theme) -> impl IntoElement {
    let (edge, lit) = (Hsla::from(theme.border_subtle), Hsla::from(theme.accent).alpha(0.7));
    let glow = field_glow(theme);
    // The choice's progress, handed from the outer transition (laid out
    // first) to the hover one inside it, which styles the card from both.
    let chosen = Rc::new(Cell::new(0f32));
    let set_chosen = chosen.clone();
    card.border_1()
        .with_hover_transition(SharedString::from(format!("{id}-hover")), transition::FOCUS, move |el, hover| {
            let t = chosen.get().max(hover * 0.6);
            el.border_color(mix(edge, lit, t)).when(t > 0., |el| el.shadow(faded(glow, t)))
        })
        .with_transition(SharedString::from(format!("{id}-chosen")), selected, transition::FOCUS, move |el, t| {
            set_chosen.set(t);
            el
        })
}

fn grid(mut cells: Vec<AnyElement>, columns: usize, gap: gpui::Pixels) -> gpui::Div {
    while !cells.len().is_multiple_of(columns) {
        cells.push(div().into_any_element());
    }
    let mut rows = Vec::new();
    while !cells.is_empty() {
        let rest = cells.split_off(columns);
        let row = std::mem::replace(&mut cells, rest);
        rows.push(div().flex().gap(gap).children(row.into_iter().map(|c| div().flex_1().flex_basis(px(0.)).min_w_0().child(c))));
    }
    div().flex().flex_col().gap(gap).children(rows)
}

fn link_button(id: &'static str, label: &'static str, theme: &Theme) -> Button {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(30.))
        .px_3p5()
        .rounded_full()
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover_bg(theme.hover)
        .hover(|s| s.text_color(theme.text))
        .child(label)
}

fn keymap_blurb(base: BaseKeymap) -> &'static str {
    match base {
        BaseKeymap::JetBrains => "IntelliJ, WebStorm, PyCharm…",
        BaseKeymap::VSCode => "Visual Studio Code and its forks",
        BaseKeymap::SublimeText => "Sublime Text",
        BaseKeymap::Zed => "Zed's default keymap",
    }
}

/// A few lines of code in a theme's own colors.
fn code_preview(t: &Theme) -> impl IntoElement {
    let s = &t.syntax;
    let line = |indent: usize, parts: Vec<(&'static str, Rgba)>| {
        div()
            .flex()
            .pl(px(indent as f32 * 14.))
            .children(parts.into_iter().map(|(text, color)| div().text_color(color).child(text)))
    };
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .h(px(96.))
        .px_3()
        .py(px(10.))
        .rounded_t(px(11.))
        .bg(t.background)
        .font_family(t.mono_font.clone())
        .text_size(px(11.))
        .line_height(px(16.))
        .child(line(0, vec![("fn ", s.keyword), ("greet", s.function), ("(", s.punctuation), ("name", s.plain), (": ", s.punctuation), ("&str", s.type_), (") {", s.punctuation)]))
        .child(line(1, vec![("// say hello", s.comment)]))
        .child(line(1, vec![("let ", s.keyword), ("n", s.plain), (" = ", s.operator), ("42", s.number), (";", s.punctuation)]))
        .child(line(1, vec![("println!", s.function), ("(", s.punctuation), ("\"hi {name}\"", s.string), (");", s.punctuation)]))
        .child(line(0, vec![("}", s.punctuation)]))
}
