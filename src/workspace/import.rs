//! Import Settings… (Find Action, File menu, Welcome Setup): picks an editor
//! found on this computer (`import::find_sources`), shows what importing its
//! setup would change (`import::plan`) and, once that's confirmed, writes it:
//! settings and per-language settings into `settings.json`, shortcuts into
//! `keymap.json`, snippet files into the snippets folder, and installs the
//! registry's extensions for the languages its extensions brought.

use super::*;
use crate::import::{self, Plan, Source};
use crate::picker::{Choice, Picker, PickerEvent, PickerMode};

impl Workspace {
    pub(super) fn import_settings(&mut self, _: &ImportSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let finding = cx.background_executor().spawn(async move {
            let home = crate::clone_dialog::home_dir().unwrap_or_default();
            import::find_sources(&import::config_root(&home), &home)
        });
        cx.spawn_in(window, async move |this, cx| {
            let sources = finding.await;
            this.update_in(cx, |ws, window, cx| ws.pick_import_source(sources, window, cx)).ok();
        })
        .detach();
    }

    fn pick_import_source(&mut self, sources: Vec<Source>, window: &mut Window, cx: &mut Context<Self>) {
        if sources.is_empty() {
            self.toast(ToastKind::Info, "No VS Code or JetBrains settings found", Some("Kuana looks in their usual settings folders.".into()), cx);
            return;
        }
        let choices = sources
            .iter()
            .enumerate()
            .map(|(ix, source)| Choice {
                id: ix.to_string(),
                file: None,
                label: source.name.clone().into(),
                detail: Some(crate::clone_dialog::display_path(&source.dir).into()),
                icon: Some(Icon::Download.path().into()),
            })
            .collect();
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices("Import Settings".into(), "Import from", choices, cx);
            picker.set_title_icon(Icon::Download, cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, move |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::Chosen(id) => {
                let source = id.parse::<usize>().ok().and_then(|ix| sources.get(ix)).cloned();
                ws.close_modal(window, cx);
                if let Some(source) = source {
                    ws.preview_import(source, window, cx);
                }
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    /// What importing `source` would change, to confirm before it's written.
    fn preview_import(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        let themes: Vec<String> =
            cx.try_global::<crate::theme::ThemeRegistry>().map(|r| r.entries.iter().map(|e| e.theme.name.to_string()).collect()).unwrap_or_default();
        let snippets = crate::snippets::snippets_dir();
        let reading = {
            let source = source.clone();
            cx.background_executor().spawn(async move { import::plan(&source, &themes, snippets.as_deref()) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let plan = reading.await;
            let asked = this.update_in(cx, |ws, window, cx| {
                if plan.is_empty() {
                    ws.toast(ToastKind::Info, format!("Nothing in {}'s settings maps onto Kuana's", source.name), None, cx);
                    return None;
                }
                let mut detail: Vec<String> = plan.preview().into_iter().map(|line| format!("• {line}")).collect();
                if !plan.notes.is_empty() {
                    detail.push(String::new());
                    detail.extend(plan.notes.iter().cloned());
                }
                Some(window.prompt(PromptLevel::Info, &format!("Import from {}?", source.name), Some(&detail.join("\n")), &["Import", "Cancel"], cx))
            });
            let Ok(Some(answer)) = asked else { return };
            if answer.await.ok() == Some(0) {
                this.update(cx, |ws, cx| ws.apply_import(&source, plan, cx)).ok();
            }
        })
        .detach();
    }

    /// Writes a confirmed import, then reloads settings and the keymap.
    fn apply_import(&mut self, source: &Source, plan: Plan, cx: &mut Context<Self>) {
        let mut problems: Vec<String> = Vec::new();
        for (key, value, _) in &plan.settings {
            if let Err(err) = Settings::set(key, value.clone()) {
                problems.push(format!("settings.json ({key}): {err}"));
            }
        }
        if !plan.languages.is_empty() {
            let merged = Settings::update("languages", |current| {
                let mut languages = current.and_then(|v| v.as_object().cloned()).unwrap_or_default();
                for (language, key, value) in &plan.languages {
                    let entry = languages.entry(language.clone()).or_insert_with(|| serde_json::json!({}));
                    if let Some(entry) = entry.as_object_mut() {
                        entry.insert(key.clone(), value.clone());
                    }
                }
                Some(serde_json::Value::Object(languages))
            });
            if let Err(err) = merged {
                problems.push(format!("settings.json (languages): {err}"));
            }
        }
        if !plan.bindings.is_empty()
            && let Err(err) = crate::keymap::import_bindings(&plan.bindings)
        {
            problems.push(format!("keymap.json: {err}"));
        }
        let mut copied = 0;
        if let Some(dir) = crate::snippets::snippets_dir().filter(|_| !plan.snippets.is_empty()) {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => {
                    for file in &plan.snippets {
                        let Some(name) = file.file_name() else { continue };
                        // Never over one of Kuana's own (the plan left those out; a race could add one).
                        let target = dir.join(name);
                        if target.exists() {
                            continue;
                        }
                        match std::fs::copy(file, &target) {
                            Ok(_) => copied += 1,
                            Err(err) => problems.push(format!("{}: {err}", name.to_string_lossy())),
                        }
                    }
                }
                Err(err) => problems.push(format!("{}: {err}", dir.display())),
            }
        }
        problems.extend(settings::reload_now(cx));
        problems.extend(settings::reload_keymap_now(cx));
        for (id, _) in &plan.extensions {
            self.install_suggested_extension(id, cx);
        }
        let mut done = vec![format!("{} setting{}", plan.settings.len() + plan.languages.len(), plural(plan.settings.len() + plan.languages.len()))];
        if !plan.bindings.is_empty() {
            done.push(format!("{} shortcut{}", plan.bindings.len(), plural(plan.bindings.len())));
        }
        if copied > 0 {
            done.push(format!("{copied} snippet file{}", plural(copied)));
        }
        let detail = (!plan.extensions.is_empty()).then(|| "Its extensions' languages are installing from Kuana's registry.".to_string());
        self.toast(ToastKind::Success, format!("Imported {} from {}", done.join(", "), source.name), detail, cx);
        if !problems.is_empty() {
            self.report_config_problems(problems, cx);
        }
        cx.notify();
    }
}
