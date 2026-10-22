//! GitHub Copilot in the workspace: its status bar item, signing in and out
//! (see `lsp::copilot`).

use super::*;
use crate::lsp::copilot::{CopilotStatus, SignIn};

impl Workspace {
    fn active_path(&self, cx: &App) -> Option<PathBuf> {
        self.active_editor(cx)?.read(cx).doc.read(cx).path.clone()
    }

    /// The active file's Copilot: installing, signing in, or a light when
    /// it's ready (click for Sign Out and Turn Off).
    pub(super) fn render_copilot_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.active_path(cx)?;
        let status = self.lsp.read(cx).copilot_status(&path)?;
        let dim: Hsla = theme.text_dim.into();
        let (label, color, light, tip): (String, Hsla, Hsla, String) = match &status {
            CopilotStatus::Starting => ("Copilot".into(), dim, theme.accent.into(), "GitHub Copilot is starting".into()),
            CopilotStatus::Missing(Some(installer)) => (
                "Install Copilot".into(),
                theme.warning.into(),
                theme.warning.into(),
                format!("{}, into Kuana's own folder", installer.describe()),
            ),
            CopilotStatus::Missing(None) => ("No Copilot".into(), theme.warning.into(), theme.warning.into(), "Kuana can't install Copilot here".into()),
            CopilotStatus::Installing => ("Installing Copilot…".into(), dim, theme.accent.into(), "Installing GitHub Copilot's language server".into()),
            CopilotStatus::SignedOut => ("Sign in to Copilot".into(), theme.warning.into(), theme.warning.into(), "Sign in with your GitHub account".into()),
            CopilotStatus::SigningIn(code) => (format!("Copilot code {code}"), theme.accent.into(), theme.accent.into(), "Waiting for the code on GitHub. Click to start over.".into()),
            CopilotStatus::Ready { user, problem: None } => ("Copilot".into(), dim, theme.success.into(), format!("GitHub Copilot, signed in as {user}")),
            CopilotStatus::Ready { problem: Some(problem), .. } => ("Copilot".into(), dim, theme.warning.into(), problem.clone()),
            CopilotStatus::NotAuthorized(user) => (
                "Copilot unavailable".into(),
                theme.warning.into(),
                theme.warning.into(),
                format!("{user} has no GitHub Copilot plan. Click to sign out."),
            ),
            CopilotStatus::Failed(message) => ("Copilot stopped".into(), theme.error.into(), theme.error.into(), format!("{message}. Click to restart.")),
        };
        Some(
            div()
                .id("copilot-status")
                .flex()
                .items_center()
                .gap_1p5()
                .cursor_pointer()
                .text_color(color)
                .child(div().size(px(6.)).rounded_full().bg(light).shadow(crate::ui::glow(theme, light.alpha(0.6), 4., 0.)))
                .child(label)
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .on_click(cx.listener(move |ws, _, window, cx| match &status {
                    CopilotStatus::Missing(Some(_)) => {
                        if let Some(path) = ws.active_path(cx) {
                            ws.lsp.update(cx, |s, cx| s.install_copilot(&path, cx));
                        }
                    }
                    CopilotStatus::SignedOut | CopilotStatus::SigningIn(_) => ws.copilot_sign_in(&CopilotSignIn, window, cx),
                    CopilotStatus::Ready { .. } | CopilotStatus::NotAuthorized(_) => ws.copilot_options(window, cx),
                    CopilotStatus::Failed(_) => ws.restart_language_server(&RestartLanguageServer, window, cx),
                    _ => {}
                }))
                .into_any_element(),
        )
    }

    /// Signs in with GitHub's device flow: shows the code, then copies it
    /// and has Copilot open github.com, where it's entered.
    pub(super) fn copilot_sign_in(&mut self, _: &CopilotSignIn, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_path(cx).filter(|path| self.lsp.read(cx).copilot_status(path).is_some()) else {
            self.toast(ToastKind::Info, "Copilot isn't on for this file", Some("Choose GitHub Copilot under Settings → Editor → Edit predictions.".into()), cx);
            return;
        };
        let start = self.lsp.update(cx, |s, cx| s.start_copilot_sign_in(&path, cx));
        cx.spawn_in(window, async move |ws, cx| {
            let (code, uri, command) = match start.await {
                Ok(SignIn::Code { code, uri, command }) => (code, uri, command),
                Ok(SignIn::Done) => {
                    ws.update(cx, |ws, cx| ws.toast(ToastKind::Success, "Signed in to GitHub Copilot", None, cx)).ok();
                    return;
                }
                Err(err) => {
                    ws.update(cx, |ws, cx| ws.toast(ToastKind::Error, "Couldn't sign in to Copilot", Some(err), cx)).ok();
                    return;
                }
            };
            let detail = format!("Your code is {code}. Copy it and enter it on the GitHub page that opens.");
            let Ok(answer) = ws.update_in(cx, |_, window, cx| {
                window.prompt(PromptLevel::Info, "Sign in to GitHub Copilot", Some(&detail), &["Copy Code and Open GitHub", "Cancel"], cx)
            }) else {
                return;
            };
            if answer.await != Ok(0) {
                ws.update(cx, |ws, cx| ws.lsp.update(cx, |s, cx| s.cancel_copilot_sign_in(&path, cx))).ok();
                return;
            }
            let Ok(finish) = ws.update(cx, |ws, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                // The page opens through Copilot; its address is here in case it doesn't.
                ws.toast(ToastKind::Info, format!("Code {code} copied"), Some(format!("Enter it at {uri}")), cx);
                ws.lsp.update(cx, |s, cx| s.finish_copilot_sign_in(&path, command, cx))
            }) else {
                return;
            };
            let result = finish.await;
            ws.update(cx, |ws, cx| match result {
                Ok(()) => ws.toast(ToastKind::Success, "Signed in to GitHub Copilot", None, cx),
                Err(err) => ws.toast(ToastKind::Error, "Copilot's sign-in didn't finish", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn copilot_sign_out(&mut self, _: &CopilotSignOut, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_path(cx) else { return };
        let task = self.lsp.update(cx, |s, cx| s.copilot_sign_out(&path, cx));
        cx.spawn(async move |ws, cx| {
            let result = task.await;
            ws.update(cx, |ws, cx| match result {
                Ok(()) => ws.toast(ToastKind::Success, "Signed out of GitHub Copilot", None, cx),
                Err(err) => ws.toast(ToastKind::Error, "Couldn't sign out of Copilot", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// The status item's click when Copilot is signed in: sign out, or turn
    /// suggestions off (back to no provider).
    fn copilot_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Info, "GitHub Copilot", None, &["Sign Out", "Turn Off Copilot", "Cancel"], cx);
        cx.spawn_in(window, async move |ws, cx| {
            let answer = answer.await;
            ws.update_in(cx, |ws, window, cx| match answer {
                Ok(0) => ws.copilot_sign_out(&CopilotSignOut, window, cx),
                Ok(1) => {
                    if let Err(err) = Settings::remove("edit_prediction_provider") {
                        ws.toast(ToastKind::Error, "Couldn't change settings.json", Some(err.to_string()), cx);
                        return;
                    }
                    settings::reload_now(cx);
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }
}
