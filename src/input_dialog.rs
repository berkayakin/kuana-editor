//! A small modal asking for one line of text (new file names, renames,
//! breakpoint conditions), optionally with a checkbox under it.

use std::ops::Range;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Newline},
    theme::Theme,
    ui::{icon_el, input_box, modal_panel},
};

pub enum InputDialogEvent {
    Confirm(String),
    Dismissed,
}

pub struct InputDialog {
    title: SharedString,
    icon: Icon,
    hint: SharedString,
    input: Entity<Editor>,
    error: Option<String>,
    /// Empty text is an answer (it clears something) rather than an error.
    allow_empty: bool,
    /// A checkbox under the field: its label and whether it's checked.
    option: Option<(SharedString, bool)>,
    /// A password: shown as bullets, and taken as typed (spaces included).
    secret: bool,
}

impl EventEmitter<InputDialogEvent> for InputDialog {}

impl InputDialog {
    /// `select` is the part of `initial` to preselect (e.g. a file's stem).
    pub fn new(
        title: impl Into<SharedString>,
        icon: Icon,
        hint: impl Into<SharedString>,
        initial: &str,
        select: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::single_line("", cx));
        input.update(cx, |e, cx| {
            e.set_text(initial, cx);
            if let Some(range) = select {
                e.select_range(range, cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        // Clear a stale error as soon as the user edits the name.
        cx.subscribe(&input, |this: &mut Self, _, _: &EditorEvent, cx| {
            if this.error.take().is_some() {
                cx.notify();
            }
        })
        .detach();
        Self { title: title.into(), icon, hint: hint.into(), input, error: None, allow_empty: false, option: None, secret: false }
    }

    /// A password field: what's typed shows as bullets.
    pub fn secret(mut self, cx: &mut Context<Self>) -> Self {
        self.input.update(cx, |e, cx| e.set_masked(true, cx));
        self.secret = true;
        self
    }

    pub fn allow_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    /// Adds a checkbox ("Include untracked files"), checked or not to start with.
    pub fn with_option(mut self, label: impl Into<SharedString>, checked: bool) -> Self {
        self.option = Some((label.into(), checked));
        self
    }

    /// Whether the checkbox is there and checked.
    pub fn option_checked(&self) -> bool {
        self.option.as_ref().is_some_and(|(_, checked)| *checked)
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        let text = if self.secret { text } else { text.trim().to_string() };
        if text.is_empty() && !self.allow_empty {
            self.set_error("Enter a name".into(), cx);
        } else {
            cx.emit(InputDialogEvent::Confirm(text));
        }
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputDialogEvent::Dismissed);
    }
}

impl Focusable for InputDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for InputDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        modal_panel(cx)
            .key_context("InputDialog")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .w(px(420.))
            .p_3()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(icon_el(self.icon, px(13.), theme.text_muted.into()))
                    .child(self.title.clone()),
            )
            .child(input_box(&self.input, None, window, cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(if self.error.is_some() { theme.error } else { theme.text_dim })
                    .child(self.error.clone().unwrap_or_else(|| self.hint.to_string())),
            )
            .when_some(self.option.clone(), |el, (label, checked)| {
                el.child(
                    div()
                        .id("input-option")
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        // Clicking it leaves the keyboard in the field, so Enter still confirms.
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some((_, checked)) = &mut this.option {
                                *checked = !*checked;
                            }
                            cx.notify();
                        }))
                        .child(icon_el(
                            if checked { Icon::SquareCheck } else { Icon::Square },
                            px(14.),
                            if checked { theme.accent.into() } else { theme.text_dim.into() },
                        ))
                        .child(label),
                )
            })
    }
}
