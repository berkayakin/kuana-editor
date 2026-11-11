//! The status bar.

use super::*;

impl Workspace {
    /// The active file's language server: its name, what it's busy with, or
    /// what's wrong (click to copy the install command or to restart).
    /// `chars`: the most of its label shown; `None`, just its light (the
    /// tooltip names it).
    pub(super) fn render_lsp_status(&self, theme: &Theme, chars: Option<usize>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.active_editor(cx)?.read(cx).doc.read(cx).path.clone()?;
        let status = self.lsp.read(cx).status(&path)?;
        let dim: Hsla = theme.text_dim.into();
        let (label, color, tip): (String, Hsla, Option<String>) = match &status {
            LspStatus::Starting => ("Starting language server…".into(), dim, None),
            LspStatus::Busy(work) => (work.clone(), dim, Some(work.clone())),
            LspStatus::Ready(name) => (name.clone(), dim, chars.is_none().then(|| name.clone())),
            LspStatus::Missing { installer: Some(installer), .. } => (
                format!("Install {}", installer.command),
                theme.warning.into(),
                Some(format!("No language server yet. {}, into Kuana's own folder; one you install yourself is used instead.", installer.describe())),
            ),
            LspStatus::Missing { command, installer: None } => {
                ("No language server".into(), theme.warning.into(), Some(format!("Install one with `{command}`. Click to copy.")))
            }
            LspStatus::Installing(command) => (format!("Installing {command}…"), dim, None),
            LspStatus::Failed(message) => ("Language server stopped".into(), theme.error.into(), Some(format!("{message}. Click to restart."))),
        };
        let tip = tip.or_else(|| chars.is_none().then(|| label.clone()));
        let label = chars.map(|chars| ellipsize(&label, chars));
        // A small light: green when ready, the accent while working.
        let light: Hsla = match &status {
            LspStatus::Ready(_) => theme.success.into(),
            LspStatus::Starting | LspStatus::Busy(_) | LspStatus::Installing(_) => theme.accent.into(),
            LspStatus::Missing { .. } => theme.warning.into(),
            LspStatus::Failed(_) => theme.error.into(),
        };
        Some(
            div()
                .id("lsp-status")
                .flex()
                .items_center()
                .gap_1p5()
                .text_color(color)
                .child(
                    div().size(px(6.)).rounded_full().bg(light).shadow(crate::ui::glow(theme, light.alpha(0.6), 4., 0.)),
                )
                .when(matches!(status, LspStatus::Missing { .. } | LspStatus::Failed(_)), |el| el.cursor_pointer())
                .children(label)
                .when_some(tip, |el, tip| el.tooltip(move |_, cx| tooltip_view(tip.clone(), cx)))
                .on_click(cx.listener(move |ws, _, window, cx| match &status {
                    LspStatus::Missing { installer: Some(_), .. } => {
                        if let Some(path) = ws.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()) {
                            ws.lsp.update(cx, |s, cx| s.install_server(&path, cx));
                        }
                    }
                    LspStatus::Missing { command: install, installer: None } => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(install.to_string()));
                        ws.toast(
                            ToastKind::Success,
                            "Install command copied",
                            Some(format!("Run `{install}` in a terminal, then restart the language server.")),
                            cx,
                        );
                    }
                    LspStatus::Failed(_) => ws.restart_language_server(&RestartLanguageServer, window, cx),
                    _ => {}
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_statusbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::chrome_theme(Theme::get(cx));
        let fit = StatusFit::for_width(f32::from(self.viewport.width));
        let lsp_item = self.render_lsp_status(&theme, fit.lsp_chars, cx);
        let copilot_item = self.render_copilot_status(&theme, cx);
        let active_editor = self.active_editor(cx);
        let editor = active_editor.as_ref().map(|e| e.read(cx));
        // The file's problems and the last Run's (both counted already).
        let run_counts = self.problems.read(cx).run_counts(editor.map(|e| e.doc.read(cx)));
        let count = |sev: Severity| editor.map_or(0, |e| e.doc.read(cx).severity_count(sev)) + run_counts[sev as usize];
        let (errors, warnings) = (count(Severity::Error), count(Severity::Warning));

        let item = |child: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_full()
                .px_2()
                .child(child)
        };
        let counter = |icon: Icon, n: usize, color: Hsla| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .when(n > 0, |el| el.text_color(color))
                .child(icon_el(icon, px(13.), if n > 0 { color } else { theme.text_dim.into() }))
                .child(n.to_string())
        };

        let mut right: Vec<AnyElement> = Vec::new();
        if let Some(e) = editor {
            if let Some(mode) = e.vim_status() {
                right.push(
                    item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text).child(mode).into_any_element())
                        .into_any_element(),
                );
            }
            let (line, col) = e.cursor_position(cx);
            let selected = e.selected_chars(cx);
            let cursors = e.selections().len();
            let pos = match () {
                _ if fit.short_position => format!("{line}:{col}"),
                _ if cursors > 1 => format!("Ln {line}, Col {col}  ({cursors} cursors)"),
                _ if selected > 0 => format!("Ln {line}, Col {col}  ({selected} selected)"),
                _ => format!("Ln {line}, Col {col}"),
            };
            right.push(
                item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text_muted).child(pos).into_any_element())
                    .into_any_element(),
            );
            if let Some((name, error)) = e.external_linter_status() {
                let (icon, color, label) = match error {
                    None => (Icon::CircleCheck, Hsla::from(theme.success), name.to_string()),
                    Some(_) => (Icon::TriangleAlert, Hsla::from(theme.warning), format!("{name} failed")),
                };
                let detail = error.map(|e| e.to_string());
                right.push(
                    item(
                        div()
                            .id("external-linter")
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(icon_el(icon, px(13.), color))
                            .child(label)
                            .when_some(detail, |el, detail| {
                                el.tooltip(move |_, cx| tooltip_view(detail.clone(), cx))
                            })
                            .into_any_element(),
                    )
                    .into_any_element(),
                );
            }
            if let Some(lsp_item) = lsp_item {
                right.push(item(lsp_item).into_any_element());
            }
            if let Some(copilot_item) = copilot_item {
                right.push(item(copilot_item).into_any_element());
            }
            if fit.indent {
                right.push(item(e.indent_label().into_any_element()).into_any_element());
            }
            let menu_item = |id: &'static str, label: String, tip: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    // Same size and rounding as the docks and the problem counts.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover_bg(theme.hover)
                    .hover(|s| s.text_color(theme.text))
                    .tooltip(move |_, cx| tooltip_view(tip, cx))
                    .child(label)
            };
            if fit.file_format {
                let tab = active_editor.clone();
                right.push(
                    menu_item("line-endings", if e.doc.read(cx).uses_crlf { "CRLF".into() } else { "LF".into() }, "Line Endings")
                        .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                            if let Some(editor) = tab.clone() {
                                ws.open_line_endings_menu(editor, event.position(), window, cx);
                            }
                        }))
                        .into_any_element(),
                );
                let tab = active_editor.clone();
                right.push(
                    menu_item("encoding", e.doc.read(cx).encoding.label(), "File Encoding")
                        .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                            if let Some(editor) = tab.clone() {
                                ws.open_encoding_menu(editor, event.position(), window, cx);
                            }
                        }))
                        .into_any_element(),
                );
            }
            if let Some(label) = self.sql_connection_label(e.doc.read(cx).path.as_deref(), cx) {
                let path = e.doc.read(cx).path.clone();
                right.push(
                    menu_item("sql-connection", label, "Database connection for this file's SQL")
                        .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| ws.open_sql_connection_menu(path.clone(), event.position(), window, cx)))
                        .into_any_element(),
                );
            }
            if e.doc.read(cx).language == Language::Python
                && self.root.is_some()
                && let Some(dir) = e.doc.read(cx).path.as_deref().and_then(Path::parent)
            {
                let label = match self.shown_venv(dir) {
                    Some(venv) => crate::python::label(&venv, self.root.as_deref()),
                    None => "No venv".into(),
                };
                right.push(
                    menu_item("python-venv", label, "Python Environment")
                        .on_click(cx.listener(|ws, event: &ClickEvent, window, cx| ws.open_venv_menu(event.position(), window, cx)))
                        .into_any_element(),
                );
            }
            let (icon, color) = match e.doc.read(cx).language {
                Language::Html => (Icon::CodeXml, Hsla::from(theme.text_muted)),
                Language::Markdown => (Icon::FileText, Hsla::from(theme.text_muted)),
                Language::PlainText => (Icon::FileText, theme.text_dim.into()),
                _ => (Icon::Braces, Hsla::from(theme.text_muted)),
            };
            right.push(
                item(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(icon_el(icon, px(13.), color))
                        .when(fit.language, |el| el.child(e.doc.read(cx).language.name()))
                        .into_any_element(),
                )
                .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(STATUSBAR_HEIGHT))
            .w_full()
            .px(px(GAP))
            .text_size(px(11.5))
            .text_color(theme.text_dim)
            .child(
                div()
                    .id("diagnostic-counts")
                    .flex()
                    .items_center()
                    .gap_2()
                    // Same size and rounding as the menus on the right.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover_bg(theme.hover)
                    .hover(|s| s.text_color(theme.text_muted))
                    .on_click(cx.listener(|ws, _, window, cx| ws.toggle_problems(&ToggleProblems, window, cx)))
                    .child(counter(Icon::CircleX, errors, theme.error.into()))
                    .child(counter(Icon::TriangleAlert, warnings, theme.warning.into())),
            )
            .when_some(self.git_busy.label().cloned(), |el, busy| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .text_color(theme.text_muted)
                        .child(icon_el(Icon::Refresh, px(12.), theme.accent.into()))
                        .child(busy),
                )
            })
            .when_some(self.render_background_documents(&theme, cx), |el, item| el.child(item))
            .child(super::title_bar::window_drag_area("statusbar-free").flex_1().min_w_0())
            .when_some(self.render_downloads(&theme), |el, downloads| el.child(downloads))
            .when_some(updater::pending_version(cx).map(|v| format!("Kuana {v} is downloaded")), |el, tip| {
                el.child(
                    div()
                        .id("restart-to-update")
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(24.))
                        .px_2()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .hover_fill(theme.accent_soft, Hsla::from(theme.accent).alpha(0.24))
                        .on_click(cx.listener(|ws, _, _, cx| ws.restart_to_update(cx)))
                        .child(icon_el(Icon::Download, px(13.), theme.accent.into()))
                        .child("Restart to Update")
                        .tooltip(move |_, cx| tooltip_view(tip.clone(), cx)),
                )
            })
            // Whatever still doesn't fit is cut at its start, the status bar's
            // middle, rather than past the window's edge.
            .child(div().flex().flex_shrink().min_w_0().overflow_hidden().justify_end().children(right))
    }
}

/// What the status bar shows at the window's width: the less used items go
/// first (each is also in a menu or Find Action), then the language server's
/// name shortens to its light, the caret's place to "line:column".
#[derive(Clone, Copy)]
struct StatusFit {
    /// The language server's label; `None`, just its light.
    lsp_chars: Option<usize>,
    indent: bool,
    /// Line endings and encoding.
    file_format: bool,
    short_position: bool,
    /// The language's name beside its icon.
    language: bool,
}

impl StatusFit {
    fn for_width(width: f32) -> Self {
        Self {
            lsp_chars: if width >= 1200. {
                Some(48)
            } else if width >= 1000. {
                Some(32)
            } else if width >= 800. {
                Some(20)
            } else {
                None
            },
            indent: width >= 720.,
            file_format: width >= 600.,
            short_position: width < 600.,
            language: width >= 520.,
        }
    }
}
