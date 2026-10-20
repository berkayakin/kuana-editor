//! Running targets and debugging.

use super::*;

impl Workspace {
    // ---- Running ----------------------------------------------------------------------

    /// Runs a configuration in a Run tab, replacing the previous run of it.
    /// The editor keeps focus, like JetBrains' Run window.
    pub(super) fn run_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        // Unsaved edits would make the run disagree with what's on screen.
        for editor in self.editors.clone() {
            if editor.read(cx).doc.read(cx).is_dirty() && editor.read(cx).doc.read(cx).path.is_some() {
                editor.update(cx, |e, cx| e.save(cx)).ok();
            }
        }
        self.restore_terminals(window, cx);
        // A rerun takes the previous run's place, in whichever group it is.
        let previous =
            self.all_terminals(cx).into_iter().find(|t| t.read(cx).run().is_some_and(|(r, _)| r.name == target.name && r.command == target.command));
        let replaced = previous.and_then(|t| self.terminals.update(cx, |p, cx| p.remove(&t, cx)));
        let place = match replaced {
            Some((group, ix)) => TerminalPlace::Tab { group, ix: Some(ix) },
            None => TerminalPlace::Tab { group: self.terminals.read(cx).active_group(), ix: None },
        };
        self.last_run = Some(target.clone());
        self.schedule_session_save(cx);
        let focused = self.active_editor(cx);
        self.open_terminal(target.cwd.clone(), Some(target), place, false, window, cx);
        if let Some(editor) = focused {
            window.focus(&editor.focus_handle(cx));
        }
    }

    /// A Markdown code block's Run: its commands in a Run tab (a rerun of
    /// them takes its place), from the project folder when the file is in
    /// it, since READMEs write commands for their project's root, else from
    /// the file's folder. In Restricted Mode it asks first, showing all of
    /// it: the preview cuts long lines off.
    pub(super) fn run_snippet(&mut self, preview: &Entity<MarkdownPreview>, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let path = preview.read(cx).path(cx);
        let cwd = match (&self.root, path.as_deref().and_then(Path::parent)) {
            (Some(root), Some(dir)) if dir.starts_with(root) => root.clone(),
            (_, Some(dir)) => dir.to_path_buf(),
            _ => self.terminal_cwd(cx),
        };
        let first = command.lines().next().unwrap_or_default();
        let mut name: String = first.chars().take(40).collect();
        if name.len() < command.len() {
            name.push('…');
        }
        let target = RunTarget { name: format!("Run '{name}'"), command, cwd, row: None, rows: 0..0, debug: None, test: None };
        if !self.restricted(cx) {
            self.run_target(target, window, cx);
            return;
        }
        let confirmed = ask(window, "Run these commands?", &target.command, "Run", false, cx);
        cx.spawn_in(window, async move |this, cx| {
            if confirmed.await {
                this.update_in(cx, |ws, window, cx| ws.run_target(target, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Run (⌃R / Shift+F10): the last configuration again, or the list.
    pub(super) fn rerun(&mut self, _: &Rerun, window: &mut Window, cx: &mut Context<Self>) {
        match self.last_run.clone() {
            Some(target) => self.run_target(target, window, cx),
            None => self.run_configuration(&RunConfiguration, window, cx),
        }
    }

    /// Stop (⌘F2 / Ctrl+F2): interrupts the newest running program.
    pub(super) fn stop_run(&mut self, _: &StopRun, _: &mut Window, cx: &mut Context<Self>) {
        // A debug session is what's running, if there is one.
        if let Some(session) = self.debug.as_ref().map(|(s, _)| s.clone()).filter(|s| s.read(cx).is_active()) {
            session.update(cx, |s, cx| s.stop(cx));
            return;
        }
        let running = self.all_terminals(cx).into_iter().filter(|t| t.read(cx).run().is_some_and(|(_, s)| *s == RunState::Running)).last();
        if let Some(terminal) = running {
            terminal.update(cx, |t, cx| t.interrupt(cx));
        }
    }
}

impl Workspace {
    // ---- Debugging --------------------------------------------------------------------

    /// A gutter ▶ (Run or Debug, as a menu when both work), or Run / Debug at caret.
    pub(super) fn on_run_event(&mut self, event: &EditorRunEvent, window: &mut Window, cx: &mut Context<Self>) {
        let target = event.target.clone();
        match event.menu_at {
            Some(position) if target.debug.is_some() => {
                let name = target.name.trim_start_matches("Run ").trim_start_matches("Test ").to_string();
                let entries = vec![
                    MenuEntry::item(format!("Run {name}"), Icon::Play).shortcut(binding_label(&editor::RunAtCaret, window)),
                    MenuEntry::item(format!("Debug {name}"), Icon::Bug).shortcut(binding_label(&editor::DebugAtCaret, window)),
                ];
                let actions = vec![Some(MenuAction::Launch(target.clone(), false)), Some(MenuAction::Launch(target, true))];
                self.show_context_menu(entries, actions, position, None, window, cx);
            }
            _ if event.debug => self.debug_target(target, window, cx),
            _ => self.run_target(target, window, cx),
        }
    }

    /// Starts debugging `target` in the Debug panel; one session at a time.
    pub(super) fn debug_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        if target.debug.is_none() {
            self.toast(ToastKind::Info, "This can't be debugged", Some("Rust, Go, Python and JavaScript tests and programs can be".into()), cx);
            return;
        }
        // What's debugged is what's on screen.
        for editor in self.editors.clone() {
            if editor.read(cx).doc.read(cx).is_dirty() && editor.read(cx).doc.read(cx).path.is_some() {
                editor.update(cx, |e, cx| e.save(cx)).ok();
            }
        }
        if let Some((old, _)) = self.debug.take() {
            old.update(cx, |s, cx| s.stop(cx));
        }
        self.clear_debug_location(cx);
        let breakpoints = self.all_breakpoints(cx);
        let session = cx.new(|cx| DebugSession::new(target.clone(), breakpoints, cx));
        let subscription = cx.subscribe_in(&session, window, |ws, _, event: &DebugEvent, window, cx| ws.on_debug_event(event, window, cx));
        self.debug = Some((session, subscription));
        self.debug_active = true;
        // Debug… offers the last one again; a process attached to may be gone.
        // It's also the selected configuration (Run, the title bar's run widget), as in JetBrains.
        if !matches!(target.debug, Some(DebugSpec::Attach { .. })) {
            self.last_run = Some(target.clone());
            self.last_debug = Some(target);
            self.schedule_session_save(cx);
        }
        self.bottom = Some(BottomPanel::Debug);
        cx.notify();
    }

    pub(super) fn on_debug_event(&mut self, event: &DebugEvent, window: &mut Window, cx: &mut Context<Self>) {
        // For the run widget, which doesn't read the session (see `running`).
        self.debug_active = self.debug.as_ref().is_some_and(|(s, _)| s.read(cx).is_active());
        match event {
            DebugEvent::Stopped { path, line } => {
                self.clear_debug_location(cx);
                self.debug_location = Some((path.clone(), *line));
                self.open_at(path.clone(), Some(*line), None, window, cx);
                if let Some(editor) = self.tab_for(path, cx) {
                    editor.update(cx, |e, cx| e.set_debug_line(Some(line.saturating_sub(1) as usize), cx));
                }
                // A breakpoint hit brings Kuana forward, like JetBrains.
                window.activate_window();
            }
            DebugEvent::Resumed | DebugEvent::Ended => self.clear_debug_location(cx),
            DebugEvent::Hide => {
                self.hide_bottom(BottomPanel::Debug);
                self.focus_active(window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn clear_debug_location(&mut self, cx: &mut Context<Self>) {
        if let Some((path, _)) = self.debug_location.take()
            && let Some(editor) = self.tab_for(&path, cx)
        {
            editor.update(cx, |e, cx| e.set_debug_line(None, cx));
        }
    }

    /// Every breakpoint: open files' (which move with edits) and the rest.
    pub(super) fn all_breakpoints(&self, cx: &App) -> HashMap<PathBuf, Vec<Breakpoint>> {
        let mut all = self.breakpoints.clone();
        for editor in &self.editors {
            let e = editor.read(cx);
            // A tab not read yet has its file's breakpoints in `self.breakpoints`.
            if let Some(path) = e.doc.read(cx).path.clone().filter(|_| e.is_loaded()) {
                let breakpoints = e.breakpoints(cx);
                if breakpoints.is_empty() {
                    all.remove(&path);
                } else {
                    all.insert(path, breakpoints);
                }
            }
        }
        all
    }

    pub(super) fn breakpoints_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (path, breakpoints) = {
            let e = editor.read(cx);
            (e.doc.read(cx).path.clone(), e.breakpoints(cx))
        };
        if let (Some(path), Some((session, _))) = (path, &self.debug) {
            session.update(cx, |s, _| s.set_breakpoints(path, breakpoints));
        }
        self.schedule_session_save(cx);
    }

    /// Right-click on a line number: add or remove its breakpoint, or give it
    /// a condition or a log message.
    pub(super) fn open_breakpoint_menu(&mut self, editor: Entity<Editor>, row: usize, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = editor.read(cx).breakpoint(row, cx).is_some();
        let toggle = binding_label(&editor::ToggleBreakpoint, window);
        let entries = if existing {
            vec![
                MenuEntry::item("Remove Breakpoint", Icon::Minus).shortcut(toggle),
                MenuEntry::item("Edit Condition…", Icon::Pencil).shortcut(binding_label(&AddConditionalBreakpoint, window)),
                MenuEntry::item("Edit Log Message…", Icon::Terminal).shortcut(binding_label(&AddLogpoint, window)),
            ]
        } else {
            vec![
                MenuEntry::item("Add Breakpoint", Icon::Plus).shortcut(toggle),
                MenuEntry::item("Add Conditional Breakpoint…", Icon::Pencil).shortcut(binding_label(&AddConditionalBreakpoint, window)),
                MenuEntry::item("Add Logpoint…", Icon::Terminal).shortcut(binding_label(&AddLogpoint, window)),
            ]
        };
        let actions = [BreakpointEdit::Toggle, BreakpointEdit::Condition, BreakpointEdit::LogMessage]
            .into_iter()
            .map(|edit| Some(MenuAction::Breakpoint(edit, editor.clone(), row)))
            .collect();
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Add Conditional Breakpoint… (⇧⌘F8 / Ctrl+Shift+F8 in JetBrains): on the caret's line.
    pub(super) fn add_conditional_breakpoint(&mut self, _: &AddConditionalBreakpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::Condition, window, cx);
    }

    /// Add Logpoint…: on the caret's line.
    pub(super) fn add_logpoint(&mut self, _: &AddLogpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::LogMessage, window, cx);
    }

    pub(super) fn edit_breakpoint_at_caret(&mut self, edit: BreakpointEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        if !editor.read(cx).can_break(cx) {
            self.toast(ToastKind::Info, crate::debugger::launch::NO_DEBUGGER, None, cx);
            return;
        }
        let row = editor.read(cx).caret_row(cx);
        self.edit_breakpoint(edit, editor, row, window, cx);
    }

    /// Asks for a breakpoint's condition or log message; confirming adds the
    /// breakpoint if the line has none, and an empty text clears the option.
    pub(super) fn edit_breakpoint(&mut self, edit: BreakpointEdit, editor: Entity<Editor>, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = editor.read(cx).breakpoint(row, cx).unwrap_or(Breakpoint::at(row));
        let line = row + 1;
        let (title, hint, initial) = match edit {
            BreakpointEdit::Condition => (
                format!("Breakpoint Condition · Line {line}"),
                "Stops only when this expression is true, e.g. count > 10",
                current.condition.clone(),
            ),
            _ => (
                format!("Log Message · Line {line}"),
                "Logs instead of stopping; {expression} shows a value, e.g. count = {count}",
                current.log_message.clone(),
            ),
        };
        let initial = initial.unwrap_or_default();
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| InputDialog::new(title, Icon::Bug, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(text) = event {
                let text = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                let mut breakpoint = editor.read(cx).breakpoint(row, cx).unwrap_or(Breakpoint::at(row));
                match edit {
                    BreakpointEdit::Condition => breakpoint.condition = text,
                    _ => breakpoint.log_message = text,
                }
                editor.update(cx, |e, cx| e.set_breakpoint(breakpoint, cx));
            }
            ws.close_modal(window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    pub(super) fn debug_session(&self) -> Option<Entity<DebugSession>> {
        self.debug.as_ref().map(|(s, _)| s.clone())
    }

    /// Resume (⌥⌘R / F9; F5 in VS Code and Zed): continue, or with nothing
    /// to continue, debug the last configuration again (else at the caret).
    pub(super) fn resume(&mut self, _: &Resume, window: &mut Window, cx: &mut Context<Self>) {
        match self.debug_session() {
            Some(session) if session.read(cx).is_paused() => session.update(cx, |s, cx| s.resume(cx)),
            Some(session) if session.read(cx).is_active() => {}
            _ => match self.last_debug.clone() {
                Some(target) => self.debug_target(target, window, cx),
                None => window.dispatch_action(Box::new(editor::DebugAtCaret), cx),
            },
        }
    }

    pub(super) fn pause_program(&mut self, _: &PauseProgram, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.pause(cx));
        }
    }

    pub(super) fn step_over(&mut self, _: &StepOver, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_over(cx));
        }
    }

    pub(super) fn step_into(&mut self, _: &StepInto, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_into(cx));
        }
    }

    pub(super) fn step_out(&mut self, _: &StepOut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_out(cx));
        }
    }

    pub(super) fn toggle_debug_panel(&mut self, _: &ToggleDebugPanel, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug.is_none() {
            self.toast(ToastKind::Info, "Not debugging", Some("Start with Debug at Caret or Debug… in the Run menu".into()), cx);
            return;
        }
        self.toggle_bottom(BottomPanel::Debug);
        if !self.shows(BottomPanel::Debug) {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    pub(super) fn render_debug_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (session, _) = self.debug.as_ref()?;
        let theme = Theme::get(cx).clone();
        Some(
            area(&theme)
                .h(self.problems_height)
                .flex_shrink_0()
                .child(cached(session))
                .child(self.resize_handle(Edge::Problems, cx)),
        )
    }

    /// Run… (⌃⌥R / Alt+Shift+F10): the project's configurations and the current file.
    pub(super) fn run_configuration(&mut self, _: &RunConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(targets) = self.configurations(self.last_run.clone(), false, cx) else { return };
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Run, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modals.current {
            picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
        }
    }

    /// Debug… (⌃⌥D / Alt+Shift+F9): the configurations that can be debugged.
    pub(super) fn debug_configuration(&mut self, _: &DebugConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut targets) = self.configurations(self.last_debug.clone(), true, cx) else { return };
        targets.retain(|t| t.debug.is_some());
        if targets.is_empty() {
            self.toast(ToastKind::Info, "Nothing to debug here", Some("Rust, Go, Python and JavaScript tests and programs can be debugged".into()), cx);
            return;
        }
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Debug, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modals.current {
            picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
        }
    }

    /// Attach to Process… (⌥⇧F5 / Ctrl+Alt+F5 in JetBrains): debug a program
    /// that's already running; the adapter depends on what it runs.
    pub(super) fn attach_to_process(&mut self, _: &AttachToProcess, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(windows) {
            self.toast(ToastKind::Info, "Attaching to a process isn't supported on Windows yet", None, cx);
            return;
        }
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Attach, window, cx);
        let cwd = self.root.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_default();
        let task = cx.spawn(async move |ws, cx| {
            let processes = cx.background_executor().spawn(async move { launch::processes() }).await;
            let targets: Vec<RunTarget> = processes
                .into_iter()
                .map(|p| RunTarget {
                    name: format!("{} ({})", p.name, p.pid),
                    command: p.command,
                    cwd: cwd.clone(),
                    row: None,
                    rows: 0..0,
                    debug: Some(DebugSpec::Attach { pid: p.pid, program: p.program }),
                    test: None,
                })
                .collect();
            ws.update(cx, |ws, cx| {
                if let Some(Modal::Picker(picker)) = &ws.modals.current
                    && picker.read(cx).mode() == PickerMode::Attach
                {
                    picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
                }
            })
            .ok();
        });
        task.detach();
    }

    /// Run… and Debug…'s list: settings' configurations, the project's, the
    /// current file's; `last` first. None (after a notice) when there's nothing.
    /// What Run… (or with `debug`, Debug…) offers: settings' configurations,
    /// detected project commands, the current file's tests and mains, and for
    /// Debug the project's own debug configurations (`.vscode/launch.json`,
    /// `.zed/debug.json`).
    pub(super) fn configurations(&mut self, last: Option<RunTarget>, debug: bool, cx: &mut Context<Self>) -> Option<Vec<RunTarget>> {
        let root = self
            .root
            .clone()
            .or_else(|| self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.as_deref()?.parent().map(Path::to_path_buf)));
        let Some(root) = root else {
            self.toast(ToastKind::Info, "Open a folder or a file to run something", None, cx);
            return None;
        };
        let current = self.active_editor(cx).and_then(|e| {
            let doc = e.read(cx).doc.read(cx);
            doc.path.clone().map(|p| (p, doc.language))
        });
        let mut targets: Vec<RunTarget> = Settings::get(cx)
            .at(&root)
            .run_configurations
            .iter()
            .map(|c| RunTarget {
                name: c.name.clone(),
                command: c.command.clone(),
                cwd: c.cwd.as_ref().map(|d| root.join(d)).unwrap_or_else(|| root.clone()),
                row: None,
                rows: 0..0,
                debug: None,
                test: None,
            })
            .collect();
        if debug {
            targets.extend(launch::project_configurations(&root, current.as_ref().map(|(p, _)| p.as_path())));
        }
        targets.extend(run::project_targets(&root, current.as_ref().map(|(p, l)| (p.as_path(), *l))));
        // Then the current file's tests and mains, by name.
        if let Some(editor) = self.active_editor(cx) {
            let file = editor.update(cx, |e, cx| e.run_targets(cx));
            targets.extend(file.iter().filter(|t| !targets.iter().any(|o| o.command == t.command)).cloned().collect::<Vec<_>>());
        }
        // The last one comes first, so Enter repeats it.
        if let Some(last) = last {
            // Debug configurations have no command: those go by name.
            targets.retain(|t| !(t.command == last.command && t.cwd == last.cwd && (!t.command.is_empty() || t.name == last.name)));
            targets.insert(0, last);
        }
        if targets.is_empty() {
            self.toast(ToastKind::Info, "Nothing to run here", Some("Add run_configurations to settings.json".into()), cx);
            return None;
        }
        Some(targets)
    }

    pub(super) fn close_terminal(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal(cx) {
            self.close_terminals(vec![terminal], window, cx);
        }
    }

    /// Closes terminal tabs, asking first when that would end a command
    /// running in one (the shell isn't at its prompt) or a Run tab's program.
    /// Cancel keeps them all.
    pub(super) fn close_terminals(&mut self, terminals: Vec<Entity<TerminalView>>, window: &mut Window, cx: &mut Context<Self>) {
        let running: Vec<(SharedString, String)> =
            terminals.iter().filter_map(|t| Some((t.read(cx).title(), t.read(cx).running_process()?))).collect();
        let Some((title, process)) = running.first() else {
            for terminal in &terminals {
                self.remove_terminal(terminal, window, cx);
            }
            return;
        };
        let (message, detail) = match running.len() {
            1 if terminals.len() == 1 => (format!("Close “{title}”?"), format!("{process} is still running in it. Closing the tab ends it.")),
            1 => ("Close the terminals?".into(), format!("{process} is still running in “{title}”. Closing the tabs ends it.")),
            n => {
                let list = running.iter().map(|(title, process)| format!("{process} in “{title}”")).collect::<Vec<_>>().join(", ");
                ("Close the terminals?".into(), format!("{n} processes are still running: {list}. Closing the tabs ends them."))
            }
        };
        // Ending a process can't be undone: Return doesn't confirm.
        let answer = ask(window, &message, &detail, "Close", false, cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await {
                this.update_in(cx, |ws, window, cx| {
                    for terminal in &terminals {
                        ws.remove_terminal(terminal, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Opens a new shell in a group of its own beside the current group.
    pub(super) fn split_terminal(&mut self, _: &SplitTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_terminals(window, cx);
        let cwd = self.terminal_cwd(cx);
        let place = if self.terminals.read(cx).is_empty() {
            TerminalPlace::Tab { group: 0, ix: None }
        } else {
            TerminalPlace::GroupAfter(self.terminals.read(cx).active_group())
        };
        self.open_terminal(cwd, None, place, true, window, cx);
    }

    pub(super) fn remove_terminal(&mut self, terminal: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let was_focused = terminal.focus_handle(cx).contains_focused(window, cx);
        if self.terminals.update(cx, |p, cx| p.remove(terminal, cx)).is_none() {
            return;
        }
        if self.terminals.read(cx).is_empty() {
            self.hide_bottom(BottomPanel::Terminal);
            self.save_state(cx);
            self.focus_active(window, cx);
        } else if was_focused {
            self.focus_terminal(window, cx);
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Renames the current terminal tab (Find Action's Rename Terminal).
    pub(super) fn rename_terminal(&mut self, _: &RenameTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_terminal(cx) {
            Some(terminal) => self.open_rename_terminal(terminal, window, cx),
            None => self.toast(ToastKind::Info, "No terminal to rename", None, cx),
        }
    }

    pub(super) fn open_rename_terminal(&mut self, terminal: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let initial = terminal.read(cx).title().to_string();
        self.close_modal(window, cx);
        let hint = "Leave empty to show the shell's own title";
        let dialog =
            cx.new(|cx| InputDialog::new("Rename Terminal", Icon::Terminal, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(name) = event {
                terminal.update(cx, |t, cx| t.set_custom_title(Some(name.clone()), cx));
                ws.schedule_session_save(cx);
            }
            ws.close_modal(window, cx);
            // Back to the terminal, rather than the editor `close_modal` focuses.
            if let Some((g, ix)) = ws.terminal_position(&terminal, cx) {
                ws.activate_terminal(g, ix, window, cx);
            }
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// A terminal tab's right-click menu.
    pub(super) fn open_terminal_tab_menu(&mut self, terminal: Entity<TerminalView>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((g, ix)) = self.terminal_position(&terminal, cx) else { return };
        let count = self.terminals.read(cx).tabs(g).len();
        let run = terminal.read(cx).run().map(|(_, state)| *state);
        let renamed = terminal.read(cx).custom_title().is_some();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TerminalTabAction| Some(MenuAction::TerminalTab(action, terminal.clone()));
        add(MenuEntry::item("Rename…", Icon::Pencil), tab(TerminalTabAction::Rename));
        if renamed {
            add(MenuEntry::text("Reset Name"), tab(TerminalTabAction::ResetName));
        }
        add(MenuEntry::text("Clear"), tab(TerminalTabAction::Clear));
        match run {
            Some(RunState::Running) => add(MenuEntry::item("Stop", Icon::Stop).shortcut(binding_label(&StopRun, window)), tab(TerminalTabAction::Stop)),
            Some(RunState::Exited(_)) => add(MenuEntry::item("Rerun", Icon::Refresh), tab(TerminalTabAction::Rerun)),
            None => {}
        }
        add(MenuEntry::Separator, None);
        add(MenuEntry::item("New Terminal", Icon::Plus).shortcut(binding_label(&NewTerminal, window)), tab(TerminalTabAction::New));
        add(
            MenuEntry::item("Split Terminal", Icon::SplitRight).shortcut(binding_label(&SplitTerminal, window)),
            tab(TerminalTabAction::Split),
        );
        add(MenuEntry::Separator, None);
        add(MenuEntry::item("Close", Icon::X), tab(TerminalTabAction::Close));
        if count > 1 {
            add(MenuEntry::text("Close Others"), tab(TerminalTabAction::CloseOthers));
        }
        if ix + 1 < count {
            add(MenuEntry::text("Close Tabs to the Right"), tab(TerminalTabAction::CloseToTheRight));
        }
        if count > 1 {
            add(MenuEntry::text("Close All"), tab(TerminalTabAction::CloseAll));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    pub(super) fn run_terminal_tab_action(&mut self, action: TerminalTabAction, terminal: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((g, ix)) = self.terminal_position(&terminal, cx) else { return };
        let closing: Vec<Entity<TerminalView>> = match action {
            TerminalTabAction::Rename => return self.open_rename_terminal(terminal, window, cx),
            TerminalTabAction::ResetName => {
                terminal.update(cx, |t, cx| t.set_custom_title(None, cx));
                self.schedule_session_save(cx);
                return;
            }
            TerminalTabAction::Clear => {
                terminal.update(cx, |t, cx| t.clear_all(cx));
                return self.activate_terminal(g, ix, window, cx);
            }
            TerminalTabAction::Stop => {
                terminal.update(cx, |t, cx| t.interrupt(cx));
                return;
            }
            TerminalTabAction::Rerun => {
                if let Some((target, _)) = terminal.read(cx).run().cloned() {
                    self.run_target(target, window, cx);
                }
                return;
            }
            TerminalTabAction::New => {
                self.terminals.update(cx, |p, _| p.set_active_group(g));
                return self.new_terminal(&NewTerminal, window, cx);
            }
            TerminalTabAction::Split => {
                self.terminals.update(cx, |p, _| p.set_active_group(g));
                return self.split_terminal(&SplitTerminal, window, cx);
            }
            TerminalTabAction::Close => vec![terminal],
            TerminalTabAction::CloseOthers => self.terminals.read(cx).tabs(g).iter().filter(|t| **t != terminal).cloned().collect(),
            TerminalTabAction::CloseToTheRight => self.terminals.read(cx).tabs(g)[ix + 1..].to_vec(),
            TerminalTabAction::CloseAll => self.terminals.read(cx).tabs(g).to_vec(),
        };
        if action != TerminalTabAction::CloseAll && action != TerminalTabAction::Close {
            // The tab that was right-clicked stays, as the current one.
            self.activate_terminal(g, ix, window, cx);
        }
        self.close_terminals(closing, window, cx);
    }
}

impl Workspace {
    // ---- Test results -----------------------------------------------------------------

    /// A test run's results: the gutter's marks (every editor), a tab in the
    /// Find panel with Rerun Failed and Rerun, and a notification that says
    /// how it went (the panel isn't shown over the run's output unasked).
    pub(super) fn show_test_run(&mut self, target: RunTarget, run: crate::test_run::TestRun, cx: &mut Context<Self>) {
        use crate::{results_panel::TabButton, test_run};
        let mut statuses = (*self.test_statuses).clone();
        test_run::record(&mut statuses, &run);
        self.test_statuses = Arc::new(statuses);
        for editor in self.editors.clone() {
            let statuses = self.test_statuses.clone();
            editor.update(cx, |e, cx| e.set_test_statuses(statuses, cx));
        }
        let this = cx.entity().downgrade();
        let run_button = |label: &str, icon: Icon, target: RunTarget| {
            let this = this.clone();
            TabButton {
                icon,
                label: label.to_string().into(),
                on_click: std::rc::Rc::new(move |window, cx| {
                    this.update(cx, |ws, cx| ws.run_target(target.clone(), window, cx)).ok();
                }),
            }
        };
        let mut buttons = Vec::new();
        if let Some(failed) = test_run::rerun_failed(&target, &run) {
            buttons.push(run_button("Rerun Failed", Icon::Refresh, failed));
        }
        buttons.push(run_button("Rerun", Icon::Play, target.clone()));
        let failed = run.count(test_run::TestStatus::Failed);
        let summary = run.summary();
        let title = format!("{}: {summary}", target.name);
        self.results.update(cx, |p, cx| p.show(title.into(), test_run::tree(&run), buttons, false, cx));
        self.has_results = true;
        let (kind, message) = match failed {
            0 => (ToastKind::Success, format!("Tests passed: {summary}")),
            _ => (ToastKind::Error, format!("Tests failed: {summary}")),
        };
        let button = (!self.shows(BottomPanel::Results)).then_some(ToastButton::ShowTestResults);
        self.toast_with_button(kind, message, None, button, cx);
        self.last_tests = Some((target, run));
        cx.notify();
    }

    /// Rerun Failed Tests: the last test run's failed tests, the way it ran them.
    pub(super) fn rerun_failed_tests(&mut self, _: &RerunFailedTests, window: &mut Window, cx: &mut Context<Self>) {
        let rerun = self.last_tests.as_ref().and_then(|(target, run)| crate::test_run::rerun_failed(target, run));
        match rerun {
            Some(target) => self.run_target(target, window, cx),
            None => self.toast(ToastKind::Info, "No failed tests to run again", None, cx),
        }
    }
}
