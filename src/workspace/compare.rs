//! Compare anything: a file (as it is, unsaved edits included) with the
//! clipboard, another file, or itself at a branch or tag, in a diff tab
//! (`DiffSource::Compare`). From the editor's and the file tree's menus and
//! Find Action; the file compared is the diff's "after" side, which follows
//! its edits.

use super::*;
use crate::{
    diff_view::Compared,
    picker::{Choice, Picker, PickerEvent, PickerMode},
};

impl Workspace {
    /// The active tab's file, for the Compare actions: an editor's, or the
    /// file a diff tab is of (compare it with something else next).
    fn file_to_compare(&mut self, cx: &mut Context<Self>) -> Option<PathBuf> {
        let path = self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()).or_else(|| {
            let item = self.active_item(cx)?;
            Some(item.diff()?.read(cx).file()).filter(|p| p.is_file())
        });
        if path.is_none() {
            self.toast(ToastKind::Info, "Open a file to compare", None, cx);
        }
        path
    }

    pub(super) fn compare_with_clipboard_action(&mut self, _: &CompareWithClipboard, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.file_to_compare(cx) {
            self.compare_with_clipboard(path, window, cx);
        }
    }

    pub(super) fn compare_with_file_action(&mut self, _: &CompareWithFile, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.file_to_compare(cx) {
            self.pick_file_to_compare(path, window, cx);
        }
    }

    pub(super) fn compare_with_branch_action(&mut self, _: &CompareWithBranch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.file_to_compare(cx) {
            self.pick_revision_to_compare(path, window, cx);
        }
    }

    /// The clipboard's text → `path`.
    pub(super) fn compare_with_clipboard(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            self.toast(ToastKind::Info, "The clipboard holds no text", None, cx);
            return;
        };
        let with = Compared::Text { label: "Clipboard".into(), text: text.into() };
        self.show_diff(DiffSource::Compare { path, with }, window, cx);
    }

    /// `other` → `path`. An open file with unsaved edits is compared as it
    /// is now; otherwise from disk (each reload reads it again).
    pub(super) fn compare_files(&mut self, path: PathBuf, other: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path == other {
            self.toast(ToastKind::Info, "That's the same file", None, cx);
            return;
        }
        let open = self.documents.read(cx).get(&other, cx).filter(|d| d.read(cx).is_dirty() && !d.read(cx).is_stand_in());
        let with = match open {
            Some(doc) => {
                let label = self.root.as_ref().and_then(|r| other.strip_prefix(r).ok()).unwrap_or(&other).to_string_lossy().replace('\\', "/");
                Compared::Text { label: format!("{label} (unsaved)"), text: doc.read(cx).rope().to_string().into() }
            }
            None => Compared::File(other),
        };
        self.show_diff(DiffSource::Compare { path, with }, window, cx);
    }

    /// Compare With…: Go to File's list picks the other file.
    pub(super) fn pick_file_to_compare(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let files = if self.root.is_some() {
            self.project.openable()
        } else {
            Arc::new(self.items(cx).iter().filter(|t| t.is_file()).filter_map(|t| t.path(cx)).collect())
        };
        let name = file_name(&path);
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Files, self.root.clone(), files, self.recent.clone(), Arc::new(Vec::new()), window, cx);
            picker.set_title(format!("Compare {name} With").into(), cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, move |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::OpenFile(other) => {
                let other = other.clone();
                ws.close_modal(window, cx);
                ws.compare_files(path.clone(), other, window, cx);
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    /// Compare with Branch…: the repository's branches and tags to pick from.
    pub(super) fn pick_revision_to_compare(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx).filter(|r| path.starts_with(r)) else {
            self.toast(ToastKind::Info, "The file isn't in a Git repository", None, cx);
            return;
        };
        cx.spawn_in(window, async move |ws, cx| {
            let read = repo.clone();
            let (branches, tags) = cx.background_executor().spawn(async move { (git::branches(&read), git::tags(&read)) }).await;
            ws.update_in(cx, |ws, window, cx| {
                let branches = match branches {
                    Ok(b) => b,
                    Err(e) => {
                        ws.toast(ToastKind::Error, "Couldn't list the branches", Some(e), cx);
                        return;
                    }
                };
                let mut choices: Vec<Choice> = branches
                    .into_iter()
                    .map(|b| Choice {
                        id: b.name.clone(),
                        label: b.name.into(),
                        detail: Some(if b.current { "current" } else if b.remote { "remote" } else { "local" }.into()),
                        icon: Some(Icon::GitBranch.path().into()),
                        file: None,
                    })
                    .collect();
                choices.extend(tags.unwrap_or_default().into_iter().map(|t| Choice {
                    id: t.clone(),
                    label: t.into(),
                    detail: Some("tag".into()),
                    icon: Some(Icon::Tag.path().into()),
                    file: None,
                }));
                let name = file_name(&path);
                let picker = cx.new(|cx| {
                    let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
                    picker.set_choices(format!("Compare {name} with Branch or Tag").into(), "Filter branches and tags", choices, cx);
                    picker.set_title_icon(Icon::GitBranch, cx);
                    picker
                });
                let subscription = cx.subscribe_in(&picker, window, move |ws, _, event: &PickerEvent, window, cx| match event {
                    PickerEvent::Chosen(rev) => {
                        let rev = rev.clone();
                        ws.close_modal(window, cx);
                        ws.compare_with_revision(path.clone(), &repo, rev, window, cx);
                    }
                    PickerEvent::Dismissed => ws.close_modal(window, cx),
                    _ => {}
                });
                ws.modals.open(Modal::Picker(picker), subscription);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The file at `rev` → `path`.
    pub(super) fn compare_with_revision(&mut self, path: PathBuf, repo: &Path, rev: String, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(rel) = path.strip_prefix(repo) else { return };
        let rel = rel.to_string_lossy().replace('\\', "/");
        self.show_diff(DiffSource::Compare { path, with: Compared::Revision { rev, rel } }, window, cx);
    }

    /// The file tree's Compare with Editor: the tree's file → the active tab's.
    pub(super) fn compare_with_editor(&mut self, other: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.file_to_compare(cx) {
            self.compare_files(path, other, window, cx);
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
