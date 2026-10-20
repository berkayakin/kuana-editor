//! Terminal tabs and groups in the bottom panel.

use super::*;

impl Workspace {
    // ---- Terminal -------------------------------------------------------------------

    /// ⌥F12: opens the terminal, focuses it, or (when focused) hides it.
    pub(super) fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.all_terminals(cx).iter().any(|t| t.focus_handle(cx).contains_focused(window, cx));
        self.set_terminal_shown(!(self.shows(BottomPanel::Terminal) && focused), window, cx);
    }

    /// Shows the terminal panel (focused, its shells started) or hides it.
    /// The title bar's button calls this directly: lit, it hides the panel,
    /// even while the editor has focus.
    pub(super) fn set_terminal_shown(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !show {
            self.hide_bottom(BottomPanel::Terminal);
            self.focus_active(window, cx);
        } else {
            self.bottom = Some(BottomPanel::Terminal);
            if self.terminals.read(cx).is_empty() && !self.restore_terminals(window, cx) {
                self.new_terminal(&NewTerminal, window, cx);
            } else {
                self.focus_terminal(window, cx);
            }
        }
        self.save_state(cx);
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Git log in the bottom panel (⌘9 / Alt+9); again hides it.
    pub(super) fn toggle_git_log(&mut self, _: &ToggleGitLog, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows(BottomPanel::GitLog) {
            self.hide_bottom(BottomPanel::GitLog);
            self.focus_active(window, cx);
        } else {
            self.git_log.update(cx, |l, cx| l.show_path(None, cx));
            self.open_git_log(window, cx);
        }
        self.save_state(cx);
        cx.notify();
    }

    /// The active file's commits (Git → Show History).
    pub(super) fn show_file_history(&mut self, _: &ShowFileHistory, window: &mut Window, cx: &mut Context<Self>) {
        let rel = self.active_editor(cx).and_then(|e| {
            let path = e.read(cx).doc.read(cx).path.clone()?;
            Some(path.strip_prefix(self.repo.as_ref()?).ok()?.to_string_lossy().replace('\\', "/"))
        });
        let Some(rel) = rel else {
            self.toast(ToastKind::Info, "This file isn't in a Git repository", None, cx);
            return;
        };
        self.show_history_of(rel, window, cx);
    }

    /// The commits touching `rel` (repository-relative) in the Git log.
    pub(super) fn show_history_of(&mut self, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        self.git_log.update(cx, |l, cx| l.show_path(Some(rel), cx));
        self.open_git_log(window, cx);
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn open_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            self.toast(ToastKind::Info, "Not a Git repository", None, cx);
            return;
        }
        self.bottom = Some(BottomPanel::GitLog);
        self.git_log.update(cx, |l, cx| l.reload(cx));
        self.git_log.read(cx).focus_filter(window, cx);
    }

    pub(super) fn render_git_log_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        area(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(cached(&self.git_log))
            .child(self.resize_handle(Edge::Problems, cx))
    }

    /// After `open_window`'s setup: start the shell for a terminal panel that
    /// was open last time, now that the project (its folder) is known.
    pub(super) fn window_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_terminal_panel(window, cx);
    }

    /// Starts the shells of a terminal panel that shows without any (it was
    /// open last time): the project's saved ones, else a new one.
    pub(super) fn start_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shows(BottomPanel::Terminal) || !self.terminals.read(cx).is_empty() {
            return;
        }
        let focused = self.active_editor(cx);
        if !self.restore_terminals(window, cx) {
            self.new_terminal(&NewTerminal, window, cx);
        }
        // Keep the editor focused, as it was before quitting.
        if let Some(editor) = focused {
            window.focus(&editor.focus_handle(cx));
        }
    }

    /// Reopens the project's shells from last time, in their groups, folders
    /// and with their names. False when there were none to open.
    pub(super) fn restore_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.terminals.read(cx).is_empty() {
            return false;
        }
        let Some(layout) = self.terminals.update(cx, |p, _| p.take_saved()) else { return false };
        let fallback = self.terminal_cwd(cx);
        for (group, saved) in layout.groups.into_iter().enumerate() {
            for tab in saved.tabs {
                let cwd = tab.cwd.filter(|d| d.is_dir()).unwrap_or_else(|| fallback.clone());
                // Into this group, which the first shell creates.
                self.open_terminal(cwd, None, TerminalPlace::Tab { group, ix: None }, false, window, cx);
                if let Some(name) = tab.name
                    && let Some(terminal) = self.terminals.read(cx).tabs(group).last().cloned()
                {
                    terminal.update(cx, |t, cx| t.set_custom_title(Some(name), cx));
                }
            }
            self.terminals.update(cx, |p, _| p.set_group_active(group, saved.active));
        }
        self.terminals.update(cx, |p, _| p.set_active_group(layout.active));
        !self.terminals.read(cx).is_empty()
    }

    pub(super) fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal(cx) {
            window.focus(&terminal.focus_handle(cx));
        }
    }

    /// The current tab of the current group.
    pub(super) fn active_terminal(&self, cx: &App) -> Option<Entity<TerminalView>> {
        self.terminals.read(cx).active_terminal().cloned()
    }

    pub(super) fn all_terminals(&self, cx: &App) -> Vec<Entity<TerminalView>> {
        self.terminals.read(cx).all_terminals().cloned().collect()
    }

    /// (group, tab) of a terminal.
    pub(super) fn terminal_position(&self, terminal: &Entity<TerminalView>, cx: &App) -> Option<(usize, usize)> {
        self.terminals.read(cx).position(terminal)
    }

    /// Where a new shell starts: the project folder (or the current file's).
    pub(super) fn terminal_cwd(&self, cx: &App) -> PathBuf {
        self.root
            .clone()
            .or_else(|| self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.as_deref()?.parent().map(Path::to_path_buf)))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Starts a shell in a new tab of the current group.
    pub(super) fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        // The project's shells from last time come back first.
        self.restore_terminals(window, cx);
        let cwd = self.terminal_cwd(cx);
        let group = self.terminals.read(cx).active_group();
        self.open_terminal(cwd, None, TerminalPlace::Tab { group, ix: None }, true, window, cx);
    }

    /// Makes a tab current, and its group the one with the keyboard.
    pub(super) fn activate_terminal(&mut self, group: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.terminals.update(cx, |p, cx| p.activate(group, ix, window, cx));
        cx.notify();
    }

    /// A Run tab finished: what its output reports goes to Problems (a run
    /// without problems clears the last one's), and a test runner's results
    /// to the Find panel and the gutter.
    pub(super) fn collect_run_problems(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let Some((target, _)) = terminal.read(cx).run().cloned() else { return };
        let output = terminal.read(cx).output();
        let framework = crate::test_run::Framework::of(&target.command);
        let task = cx.spawn(async move |ws, cx| {
            let cwd = target.cwd.clone();
            let (problems, tests) = cx
                .background_executor()
                .spawn(async move {
                    let text = output.text();
                    let tests = framework.map(|f| crate::test_run::parse(f, &text, &cwd)).filter(|t| !t.results.is_empty());
                    (run::output_problems(&text, &cwd), tests)
                })
                .await;
            ws.update(cx, |ws, cx| {
                if let Some(tests) = tests {
                    ws.show_test_run(target.clone(), tests, cx);
                }
                let run = (!problems.is_empty()).then_some((target.name, problems));
                ws.problems.update(cx, |p, cx| p.set_run(run, cx));
                cx.notify();
            })
            .ok();
        });
        task.detach();
        cx.notify();
    }

    /// Open files may have a language now, or one with a grammar.
    /// The folder trust decides about: the project, else the active file's.
    pub(super) fn trust_folder(&self, cx: &App) -> Option<PathBuf> {
        self.root.clone().or_else(|| self.active_editor(cx)?.read(cx).doc.read(cx).path.as_deref()?.parent().map(Path::to_path_buf))
    }

    pub(super) fn restricted(&self, cx: &App) -> bool {
        self.trust_folder(cx).is_some_and(|f| !crate::trust::is_trusted(&f))
    }

    pub(super) fn project_trust(&mut self, _: &ProjectTrust, window: &mut Window, cx: &mut Context<Self>) {
        self.open_trust_dialog(window, cx);
    }

    pub(super) fn open_trust_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.trust_folder(cx) else {
            self.toast(ToastKind::Info, "Open a folder to trust it", None, cx);
            return;
        };
        self.trust_waits = false;
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| crate::trust_dialog::TrustDialog::new(folder, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &crate::trust_dialog::TrustDialogEvent, window, cx| {
            use crate::trust_dialog::TrustDialogEvent;
            match event {
                TrustDialogEvent::Trust(folder) => {
                    ws.close_modal(window, cx);
                    crate::trust::trust(folder, cx);
                }
                TrustDialogEvent::Untrust(folder) => {
                    ws.close_modal(window, cx);
                    crate::trust::untrust(folder, cx);
                }
                TrustDialogEvent::Dismissed => ws.close_modal(window, cx),
            }
        });
        self.modals.open(Modal::Trust(dialog), subscription);
        cx.notify();
    }

    /// Asks the trust question Welcome Setup held back, once nothing covers
    /// the project. Deferred: what closed Welcome Setup or a dialog may be
    /// opening another (its settings link opens Preferences, asked after).
    pub(super) fn ask_waiting_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.trust_waits {
            return;
        }
        cx.defer_in(window, |ws, window, cx| {
            if ws.trust_waits && ws.onboarding.is_none() && ws.modals.current.is_none() {
                ws.trust_waits = false;
                if ws.restricted(cx) {
                    ws.open_trust_dialog(window, cx);
                }
            }
        });
    }

    /// Starts what trust allows, or stops it.
    pub(super) fn trust_changed(&mut self, cx: &mut Context<Self>) {
        if self.restricted(cx) {
            self.lsp.update(cx, |s, _| s.shutdown_all());
        }
        for editor in self.editors.clone() {
            editor.update(cx, |e, cx| e.trust_changed(cx));
            // Starts its server when trusted now.
            self.lsp.update(cx, |s, cx| s.register(&editor, cx));
        }
        self.refresh_git(cx);
        cx.notify();
    }

    pub(super) fn extensions_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for editor in self.editors.clone() {
            if editor.update(cx, |e, cx| e.refresh_language(cx)) {
                self.lsp.update(cx, |s, cx| s.register(&editor, cx));
            }
        }
        if let Some(message) = cx.global::<extensions::ExtensionsChanged>().message.clone() {
            self.toast(ToastKind::Info, message, None, cx);
        }
        self.offer_wasi_sdk(window, cx);
        cx.notify();
    }

    /// Grammars are waiting for the WebAssembly toolchain: asks to download
    /// it, once per reload of the extensions (in one window).
    pub(super) fn offer_wasi_sdk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let waiting = extensions::waiting_for_toolchain();
        let generation = cx.global::<extensions::ExtensionsChanged>().generation;
        let Some(mb) = crate::wasi_sdk::download_mb() else { return };
        if waiting.is_empty() || crate::wasi_sdk::installing() || ASKED.swap(generation, std::sync::atomic::Ordering::SeqCst) == generation {
            return;
        }
        let detail = format!(
            "Extensions' grammars ({}) are compiled to WebAssembly, which runs them sandboxed. The compiler, wasi-sdk, is a {mb} MB download (about 170 MB on disk, in Kuana's config folder).",
            waiting.join(", ")
        );
        let rx = window.prompt(PromptLevel::Info, "Download the WebAssembly toolchain?", Some(&detail), &["Download", "Not Now"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if rx.await != Ok(0) {
                return;
            }
            let result = cx.background_executor().spawn(async { crate::wasi_sdk::install() }).await;
            this.update(cx, |ws, cx| match result {
                Ok(()) => {
                    ws.toast(ToastKind::Success, "wasi-sdk installed; building grammars", None, cx);
                    extensions::reload(cx);
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't install wasi-sdk", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Adds a terminal where `place` says. A Run tab stays open when its
    /// program ends, showing how it went. Its shell starts in the background
    /// (`TerminalPanelEvent::Failed` if it can't).
    pub(super) fn open_terminal(
        &mut self,
        cwd: PathBuf,
        run: Option<RunTarget>,
        place: TerminalPlace,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = TerminalView::create(cwd, run, cx);
        self.terminals.update(cx, |p, cx| p.add(terminal, place, cx));
        self.bottom = Some(BottomPanel::Terminal);
        if focus {
            self.focus_terminal(window, cx);
        }
        self.save_state(cx);
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// A terminal's shell couldn't start: its tab goes, with a message.
    pub(super) fn terminal_failed(&mut self, terminal: &Entity<TerminalView>, error: String, window: &mut Window, cx: &mut Context<Self>) {
        // Closing the last one hides the panel, so it doesn't start another.
        self.remove_terminal(terminal, window, cx);
        self.toast(ToastKind::Error, "Couldn't start the terminal", Some(error), cx);
    }

    /// The terminal panel in the bottom area, as tall as it is, with its grab edge.
    pub(super) fn render_terminal_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .flex()
            .h(self.problems_height)
            .flex_shrink_0()
            .child(cached(&self.terminals))
            .child(self.resize_handle(Edge::Problems, cx))
    }
}
