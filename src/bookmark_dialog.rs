//! Toggle Bookmark with Mnemonic: a grid of 0–9 and A–Z, as in JetBrains.
//! Typing one of them (or clicking it) picks it; mnemonics already used
//! elsewhere in the project are marked, and picking one moves it here.

use std::collections::HashMap;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, KeyDownEvent, Render, SharedString, Window, div,
    prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::Cancel,
    theme::Theme,
    ui::{HoverFill, icon_el, kbd, modal_panel},
};

pub enum MnemonicEvent {
    /// A mnemonic for the line (None: a plain bookmark).
    Chosen(Option<char>),
    /// The line's bookmark is to go.
    Remove,
    Dismissed,
}

pub struct MnemonicDialog {
    title: SharedString,
    /// The line's current mnemonic.
    current: Option<char>,
    /// Mnemonics used on other lines, with where ("main.rs:12").
    used: HashMap<char, String>,
    /// The line already has a bookmark (Delete removes it).
    bookmarked: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<MnemonicEvent> for MnemonicDialog {}

/// The grid's rows, as JetBrains lays them out.
const ROWS: [&str; 4] = ["1234567890", "ABCDEFGHI", "JKLMNOPQR", "STUVWXYZ"];

impl MnemonicDialog {
    pub fn new(
        title: impl Into<SharedString>,
        current: Option<char>,
        bookmarked: bool,
        used: HashMap<char, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self { title: title.into(), current, used, bookmarked, focus_handle }
    }

    /// The mnemonic a key types, if it's one.
    pub fn mnemonic_of(key: &str) -> Option<char> {
        let mut chars = key.chars();
        let c = chars.next()?;
        (chars.next().is_none() && c.is_ascii_alphanumeric()).then(|| c.to_ascii_uppercase())
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        match keystroke.key.as_str() {
            "backspace" | "delete" if self.bookmarked => cx.emit(MnemonicEvent::Remove),
            // Enter keeps the bookmark as it is (or adds a plain one).
            "enter" => cx.emit(MnemonicEvent::Chosen(self.current)),
            key => match Self::mnemonic_of(key) {
                Some(c) => cx.emit(MnemonicEvent::Chosen(Some(c))),
                None => return,
            },
        }
        cx.stop_propagation();
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(MnemonicEvent::Dismissed);
    }
}

impl Focusable for MnemonicDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MnemonicDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let cell = px(30.);
        let grid = div().flex().flex_col().gap_1().items_center().children(ROWS.iter().map(|row| {
            div().flex().gap_1().children(row.chars().map(|c| {
                let current = self.current == Some(c);
                let used = self.used.get(&c).cloned();
                let label = div()
                    .id(SharedString::from(format!("mnemonic-{c}")))
                    .size(cell)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .border_1()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(MnemonicEvent::Chosen(Some(c)))))
                    .when_some(used.clone(), |el, place| {
                        el.tooltip(move |_, cx| crate::ui::tooltip_view(format!("Used at {place}; picking it moves it here"), cx))
                    });
                if current {
                    label.border_color(theme.accent).bg(theme.accent_soft).text_color(theme.accent).child(c.to_string()).into_any_element()
                } else if used.is_some() {
                    // Taken elsewhere: still pickable, drawn filled and dim.
                    label
                        .border_color(theme.border_subtle)
                        .text_color(theme.text_dim)
                        .hover_fill(theme.hover, theme.active)
                        .child(c.to_string())
                        .into_any_element()
                } else {
                    label.border_color(theme.border_subtle).text_color(theme.text).hover_bg(theme.hover).child(c.to_string()).into_any_element()
                }
            }))
        }));
        let hint = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap_1()
            .text_size(px(12.))
            .text_color(theme.text_dim)
            .child("Type a digit or letter")
            .when(self.bookmarked, |el| el.child("·").child(kbd("⌫", cx)).child("removes the bookmark"))
            .child("·")
            .child(kbd("↩", cx))
            .child(if self.bookmarked { "keeps it" } else { "adds one without" });
        modal_panel(cx)
            .key_context("MnemonicDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::on_escape))
            .w(px(380.))
            .p_3()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(icon_el(Icon::Bookmark, px(13.), theme.text_muted.into()))
                    .child(self.title.clone()),
            )
            .child(grid)
            .child(hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_type_mnemonics() {
        assert_eq!(MnemonicDialog::mnemonic_of("a"), Some('A'));
        assert_eq!(MnemonicDialog::mnemonic_of("7"), Some('7'));
        assert_eq!(MnemonicDialog::mnemonic_of("-"), None);
        assert_eq!(MnemonicDialog::mnemonic_of("f1"), None);
        assert_eq!(MnemonicDialog::mnemonic_of("é"), None);
    }
}
