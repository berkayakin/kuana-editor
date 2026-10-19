//! Line endings and encoding (status bar menus).

use super::*;

impl Workspace {
    // ---- Line endings and encoding (status bar) ------------------------------------

    pub(super) fn open_line_endings_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let crlf = editor.read(cx).doc.read(cx).uses_crlf;
        let choice = |label: &'static str, on: bool| if on { MenuEntry::item(label, Icon::Check) } else { MenuEntry::text(label) };
        let entries = vec![
            MenuEntry::Header("Line endings".into()),
            choice("LF (macOS, Linux)", !crlf),
            choice("CRLF (Windows)", crlf),
        ];
        let actions = vec![None, Some(MenuAction::LineEndings(false, editor.clone())), Some(MenuAction::LineEndings(true, editor))];
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    pub(super) fn open_encoding_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let current = editor.read(cx).doc.read(cx).encoding;
        let mut entries = vec![MenuEntry::Header("Encoding".into())];
        let mut actions = vec![None];
        for encoding in crate::encoding::choices() {
            entries.push(match encoding == current {
                true => MenuEntry::item(encoding.menu_label(), Icon::Check),
                false => MenuEntry::text(encoding.menu_label()),
            });
            actions.push(Some(MenuAction::Encoding(encoding, editor.clone())));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The status bar's venv for the active file's folder.
    pub(super) fn shown_venv(&self, dir: &Path) -> Option<PathBuf> {
        if let Some((cached_dir, venv)) = self.venv_shown.borrow().as_ref()
            && cached_dir == dir
        {
            return venv.clone();
        }
        let venv = crate::python::venv(dir);
        self.venv_shown.replace(Some((dir.to_path_buf(), venv.clone())));
        venv
    }

    pub(super) fn open_venv_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::python::{self, Choice};
        let Some(root) = self.root.clone() else { return };
        let current = python::choice(&root);
        let check = |on: bool, label: String| if on { MenuEntry::item(label, Icon::Check) } else { MenuEntry::text(label) };
        let detected = self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.as_deref()?.parent().and_then(python::detect));
        let auto = match &detected {
            Some(venv) => format!("Auto-detect ({})", python::label(venv, Some(&root))),
            None => "Auto-detect (none found)".into(),
        };
        let mut entries = vec![MenuEntry::Header("Python Environment".into()), check(current.is_none(), auto)];
        let mut actions = vec![None, Some(MenuAction::Venv(None))];
        let mut venvs = python::candidates(&root);
        // A folder picked outside the project stays listed.
        if let Some(Choice::Venv(path)) = &current
            && !venvs.contains(path)
        {
            venvs.push(path.clone());
        }
        for venv in venvs {
            let on = current.as_ref() == Some(&Choice::Venv(venv.clone()));
            entries.push(check(on, python::label(&venv, Some(&root))));
            actions.push(Some(MenuAction::Venv(Some(Choice::Venv(venv)))));
        }
        entries.push(check(current == Some(Choice::Off), "None".into()));
        actions.push(Some(MenuAction::Venv(Some(Choice::Off))));
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::text("Choose Folder…"));
        actions.push(Some(MenuAction::ChooseVenv));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Saves the project's venv choice. New terminals and Run tabs use it;
    /// the active file's language server restarts with it.
    pub(super) fn set_venv(&mut self, choice: Option<crate::python::Choice>, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        crate::python::set_choice(&root, choice);
        self.venv_shown.take();
        self.schedule_session_save(cx);
        if let Some(path) = self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()) {
            self.lsp.update(cx, |s, cx| s.restart(&path, cx));
        }
        cx.notify();
    }

    pub(super) fn choose_venv_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Use Venv".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            this.update(cx, |ws, cx| {
                if path.join("pyvenv.cfg").is_file() {
                    ws.set_venv(Some(crate::python::Choice::Venv(path)), cx);
                } else {
                    let detail = format!("{} has no pyvenv.cfg. Pick the folder python -m venv created.", path.display());
                    ws.toast(ToastKind::Error, "Not a virtual environment", Some(detail), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Like JetBrains: reload the file as the new encoding (its text was read
    /// wrong) or convert it (the next save writes the new encoding).
    pub(super) fn choose_encoding(&mut self, encoding: TextEncoding, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let (current, dirty, saved) = {
            let doc = editor.read(cx).doc.read(cx);
            (doc.encoding, doc.is_dirty(), doc.path.is_some())
        };
        if encoding == current {
            return;
        }
        let convert = move |editor: &Entity<Editor>, cx: &mut Context<Self>| {
            editor.update(cx, |e, cx| {
                e.doc.as_mut(cx).convert_to(encoding);
                cx.emit(EditorEvent::Edited);
                cx.notify();
            })
        };
        if !saved {
            convert(&editor, cx);
            return;
        }
        let label = encoding.label();
        let message = format!("Reload or convert to {label}?");
        let detail = if dirty {
            format!("Convert saves the text as {label}. Reloading isn't possible with unsaved changes.")
        } else {
            format!("Reload reads the file again as {label}, for text that looks wrong. Convert keeps the text and saves it as {label}.")
        };
        let buttons: &[&str] = if dirty { &["Convert", "Cancel"] } else { &["Reload", "Convert", "Cancel"] };
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), buttons, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let reload = !dirty && choice == 0;
            let convert_choice = if dirty { 0 } else { 1 };
            this.update(cx, |ws, cx| {
                if reload {
                    if let Err(err) = editor.update(cx, |e, cx| e.reload_with_encoding(encoding, cx)) {
                        ws.toast(ToastKind::Error, "Couldn't reload the file", Some(err.to_string()), cx);
                    }
                } else if choice == convert_choice {
                    convert(&editor, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}
