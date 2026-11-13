//! Scratch files (`crate::scratch`): New Scratch File asks for a language
//! and opens an empty file; Scratch Files lists them, newest first, and ⌫
//! there moves one to the Trash (undoable, like the tree's deletes).

use std::time::SystemTime;

use super::*;
use crate::picker::{Choice, Picker, PickerEvent, PickerMode};

impl Workspace {
    pub(super) fn new_scratch_file(&mut self, _: &NewScratchFile, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        // The active file's language first: a scratch is often "this, but apart".
        let current = self.active_editor(cx).map(|e| e.read(cx).doc.read(cx).language.name());
        let mut languages = crate::scratch::languages();
        if let Some(at) = current.and_then(|name| languages.iter().position(|l| l.name == name)) {
            let language = languages.remove(at);
            languages.insert(0, language);
        }
        let choices = languages
            .into_iter()
            .map(|l| Choice {
                id: l.suffix.clone(),
                file: Some(PathBuf::from(format!("scratch.{}", l.suffix))),
                label: l.name.into(),
                detail: Some(format!(".{}", l.suffix).into()),
                icon: None,
            })
            .collect();
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices("New Scratch File".into(), "Language", choices, cx);
            picker.set_title_icon(Icon::FilePlus, cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::Chosen(suffix) => {
                let suffix = suffix.clone();
                ws.close_modal(window, cx);
                match crate::scratch::create(&suffix) {
                    Ok(path) => ws.open_path(path, window, cx),
                    Err(e) => ws.toast(ToastKind::Error, "Couldn't create the scratch file", Some(e.to_string()), cx),
                }
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    pub(super) fn show_scratches(&mut self, _: &ShowScratches, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let choices = scratch_choices();
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices("Scratch Files".into(), "Filter scratch files", choices, cx);
            picker.set_title_icon(Icon::File, cx);
            picker.set_removable(cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, |ws, picker, event: &PickerEvent, window, cx| match event {
            PickerEvent::Chosen(path) => {
                let path = PathBuf::from(path);
                ws.close_modal(window, cx);
                ws.open_path(path, window, cx);
            }
            PickerEvent::RemoveChoice(path) => {
                let path = PathBuf::from(path);
                let dirty = ws.tab_for(&path, cx).is_some_and(|e| e.read(cx).doc.read(cx).is_dirty());
                if dirty {
                    // Unsaved edits would be lost: ask, as the tree does.
                    ws.close_modal(window, cx);
                    ws.confirm_trash(path, window, cx);
                } else {
                    ws.move_to_trash(&path, window, cx);
                    let choices = scratch_choices();
                    picker.update(cx, |p, cx| p.refill_choices(choices, cx));
                    window.focus(&picker.focus_handle(cx));
                }
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }
}

/// The scratch files as rows: name, when last changed, the file's icon.
fn scratch_choices() -> Vec<Choice> {
    let now = SystemTime::now();
    crate::scratch::list()
        .into_iter()
        .map(|(path, modified)| Choice {
            id: path.to_string_lossy().into_owned(),
            label: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into(),
            detail: Some(ago(now, modified).into()),
            icon: None,
            file: Some(path),
        })
        .collect()
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}
