//! Asks whether to trust a project folder (see `trust.rs`), or for a trusted
//! one, offers to stop trusting it.

use std::path::{Path, PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render, Window, div,
    prelude::*, px,
};

use crate::{
    assets::Icon,
    editor::Cancel,
    theme::Theme,
    ui::{icon_el, modal_panel, text_button},
};

pub enum TrustDialogEvent {
    /// Trust this folder (the project's, or a parent of it).
    Trust(PathBuf),
    /// Stop trusting this folder.
    Untrust(PathBuf),
    Dismissed,
}

pub struct TrustDialog {
    folder: PathBuf,
    /// The trusted folder covering `folder`, if any.
    trusted_by: Option<PathBuf>,
    /// Trust the parent folder instead (every project in it).
    parent: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<TrustDialogEvent> for TrustDialog {}

impl TrustDialog {
    pub fn new(folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let trusted_by = crate::trust::trusted_by(&folder);
        Self { folder, trusted_by, parent: false, focus_handle }
    }

    /// The parent to offer, unless it's the home folder or a root, which
    /// would trust far too much.
    fn parent_folder(&self) -> Option<&Path> {
        let parent = self.folder.parent()?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        (parent.parent().is_some() && Some(parent) != home.as_deref()).then_some(parent)
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TrustDialogEvent::Dismissed);
    }
}

/// `~/…` for folders in the home folder; `box: ~/…` for a remote one.
fn display(path: &Path) -> String {
    crate::clone_dialog::display_path(path)
}

impl Focusable for TrustDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TrustDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let trusted = self.trusted_by.is_some();
        let title = if trusted { "This folder is trusted" } else { "Do you trust this folder?" };
        let (icon, icon_color) = if trusted { (Icon::ShieldCheck, theme.success) } else { (Icon::ShieldAlert, theme.warning) };
        let bullet = |text: &'static str| {
            div()
                .flex()
                .gap_2()
                .child(div().text_color(theme.text_dim).child("•"))
                .child(text)
        };
        let body = if let Some(by) = self.trusted_by.clone() {
            let detail = if by == self.folder {
                "Kuana runs its language servers, formatters, ESLint and the tools installed in it.".to_string()
            } else {
                format!("Every project in {} is trusted, this one too.", display(&by))
            };
            div().flex().flex_col().gap_2().child(detail)
        } else {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child("Tools Kuana starts on its own may run code from this folder. Until you trust it, Kuana stays in Restricted Mode:")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(bullet("Language servers don't start"))
                        .child(bullet("Formatters and ESLint don't run"))
                        .child(bullet("Tools installed in the folder (node_modules, a venv) aren't used"))
                        .child(bullet("Terminals don't activate its Python venv")),
                )
                .child(div().text_color(theme.text_muted).child("Editing, Git, Run and Debug work as usual."))
        };
        let parent = self.parent_folder().map(display);
        let footer = if let Some(by) = self.trusted_by.clone() {
            let label = if by == self.folder { "Don't Trust" } else { "Don't Trust Them" };
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(text_button("trust-close", "Close", false, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(TrustDialogEvent::Dismissed))))
                .child(text_button("trust-revoke", label, true, cx).on_click(cx.listener(move |_, _, _, cx| cx.emit(TrustDialogEvent::Untrust(by.clone())))))
        } else {
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    text_button("trust-stay", "Stay in Restricted Mode", false, cx)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(TrustDialogEvent::Dismissed))),
                )
                .child(text_button("trust-yes", "Trust Folder", true, cx).on_click(cx.listener(|this, _, _, cx| {
                    let folder = match this.parent_folder() {
                        Some(parent) if this.parent => parent.to_path_buf(),
                        _ => this.folder.clone(),
                    };
                    cx.emit(TrustDialogEvent::Trust(folder));
                })))
        };
        modal_panel(cx)
            .key_context("TrustDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_escape))
            .w(px(460.))
            .p_4()
            .gap_3()
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon_el(icon, px(18.), icon_color.into()))
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(title)),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .bg(theme.background)
                    .font_family(theme.mono_font.clone())
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .truncate()
                    .child(display(&self.folder)),
            )
            .child(body)
            .when_some(parent.filter(|_| !trusted), |el, parent| {
                el.child(
                    div()
                        .id("trust-parent")
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.parent = !this.parent;
                            cx.notify();
                        }))
                        .child(icon_el(
                            if self.parent { Icon::SquareCheck } else { Icon::Square },
                            px(15.),
                            if self.parent { theme.accent.into() } else { theme.text_dim.into() },
                        ))
                        .child(div().truncate().child(format!("Trust every project in {parent}"))),
                )
            })
            .child(footer)
    }
}
