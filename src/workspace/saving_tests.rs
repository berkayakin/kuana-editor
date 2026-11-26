//! Save, Save All and closing tabs, in a real workspace (`harness`).

use gpui::TestAppContext;

use super::harness::{self, TempProject, Ws, answer, disk_changed, prompt, wait};
use super::*;

/// A workspace with `project` open and `files` opened in tabs, in order.
fn with_files(cx: &mut TestAppContext, project: &TempProject, files: &[&str]) -> Ws {
    harness::init(cx);
    let ws = harness::workspace(cx);
    ws.open_project(cx, &project.dir);
    for name in files {
        ws.open(cx, &project.path(name));
    }
    ws
}

fn save(ws: Ws, cx: &mut TestAppContext) {
    ws.act(cx, |ws, window, cx| ws.save(&Save, window, cx));
}

fn close_tab(ws: Ws, cx: &mut TestAppContext) {
    ws.act(cx, |ws, window, cx| ws.close_tab(&CloseTab, window, cx));
}

/// Typing then Save writes the file, and the tab is clean with the text
/// the file has.
#[gpui::test]
fn save_writes_the_text_and_the_tab_is_clean(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\ntwo\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(ws.active_text(cx), "one\ntwo\n");

    ws.put_caret(cx, 4);
    ws.type_text(cx, "and a half\n");
    assert!(ws.active_dirty(cx));
    // Not written yet.
    assert_eq!(project.read("a.txt"), "one\ntwo\n");

    save(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "one\nand a half\ntwo\n");
    assert_eq!(ws.active_text(cx), project.read("a.txt"));
    assert!(!ws.active_dirty(cx));
    // The caret stays after what was typed.
    assert_eq!(ws.active_caret(cx), (3, 1));
    // Saving again with nothing changed on disk asks nothing.
    ws.type_text(cx, "!");
    save(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "one\nand a half\n!two\n");
}

/// A file another program changed while the tab has unsaved edits: Save
/// asks, and Overwrite writes the tab's text.
#[gpui::test]
fn save_over_a_change_on_disk_asks_and_overwrite_writes(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "mine ");
    disk_changed(&project.path("a.txt"), "theirs\n");

    save(ws, cx);
    let (message, _) = prompt(cx).expect("Save asks first");
    assert_eq!(message, "a.txt changed on disk");
    // Nothing is written while it asks.
    assert_eq!(project.read("a.txt"), "theirs\n");
    answer(cx, "Overwrite");
    assert_eq!(project.read("a.txt"), "mine one\n");
    assert!(!ws.active_dirty(cx));
    // Written, the file is as the tab knows it: the next Save doesn't ask.
    ws.type_text(cx, "x");
    save(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "mine xone\n");
}

/// Reload takes the file's text (Undo brings the tab's back) and writes
/// nothing.
#[gpui::test]
fn save_over_a_change_on_disk_can_reload_instead(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "mine ");
    disk_changed(&project.path("a.txt"), "theirs\n");

    save(ws, cx);
    assert_eq!(prompt(cx).map(|p| p.0).as_deref(), Some("a.txt changed on disk"));
    answer(cx, "Reload");
    assert_eq!(project.read("a.txt"), "theirs\n");
    assert_eq!(ws.active_text(cx), "theirs\n");
    assert!(!ws.active_dirty(cx));
    let editor = ws.active_editor(cx);
    editor.update(cx, |e, cx| e.undo_step(cx));
    assert_eq!(ws.active_text(cx), "mine one\n");
}

/// Cancel leaves both alone: the file keeps the other change, the tab its
/// unsaved text.
#[gpui::test]
fn save_over_a_change_on_disk_can_be_cancelled(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "mine ");
    disk_changed(&project.path("a.txt"), "theirs\n");

    save(ws, cx);
    answer(cx, "Cancel");
    assert_eq!(project.read("a.txt"), "theirs\n");
    assert_eq!(ws.active_text(cx), "mine one\n");
    assert!(ws.active_dirty(cx));
    // It asks again next time.
    save(ws, cx);
    assert_eq!(prompt(cx).map(|p| p.0).as_deref(), Some("a.txt changed on disk"));
    answer(cx, "Cancel");
}

/// A file deleted meanwhile: Save asks, Save brings it back with the
/// tab's text, Cancel leaves it deleted.
#[gpui::test]
fn save_of_a_deleted_file_asks_first(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "mine ");
    std::fs::remove_file(project.path("a.txt")).unwrap();

    save(ws, cx);
    assert_eq!(prompt(cx).map(|p| p.0).as_deref(), Some("a.txt was deleted"));
    answer(cx, "Cancel");
    assert!(!project.path("a.txt").exists());
    assert!(ws.active_dirty(cx));

    save(ws, cx);
    answer(cx, "Save");
    assert_eq!(project.read("a.txt"), "mine one\n");
    assert!(!ws.active_dirty(cx));
}

/// Save All (before Run, Debug, closing) saves every tab with edits but
/// those whose file changed on disk, and says which it left.
#[gpui::test]
fn save_tabs_leaves_files_changed_on_disk(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n"), ("d.txt", "d\n")]);
    let ws = with_files(cx, &project, &["a.txt", "b.txt", "c.txt", "d.txt"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        let editor = ws.editor_for(cx, &project.path(name)).unwrap();
        editor.update(cx, |e, cx| e.replace_ranges(vec![(0..0, "new ".into())], cx));
    }
    disk_changed(&project.path("b.txt"), "theirs\n");

    ws.act(cx, |ws, _, cx| ws.save_tabs(cx));
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "new a\n");
    assert_eq!(project.read("b.txt"), "theirs\n");
    assert_eq!(project.read("c.txt"), "new c\n");
    // Without edits: not written.
    assert_eq!(project.read("d.txt"), "d\n");
    let dirty = |name: &str, cx: &mut TestAppContext| {
        let editor = ws.editor_for(cx, &project.path(name)).unwrap();
        editor.read_with(cx, |e, cx| e.doc.read(cx).is_dirty())
    };
    assert!(!dirty("a.txt", cx));
    assert!(dirty("b.txt", cx));
    assert!(!dirty("c.txt", cx));
    assert!(ws.toasts(cx).contains(&"b.txt changed on disk, so it wasn't saved".to_string()));
}

/// Closing a tab with unsaved edits asks; Save writes the file and closes
/// the tab.
#[gpui::test]
fn closing_an_edited_tab_asks_and_save_writes_it(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = with_files(cx, &project, &["a.txt", "b.txt"]);
    ws.set_auto_save(cx, AutoSave::Off);
    ws.type_text(cx, "new ");

    close_tab(ws, cx);
    let (message, _) = prompt(cx).expect("closing asks");
    assert_eq!(message, "Do you want to save the changes you made to b.txt?");
    // Still there while it asks.
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);
    answer(cx, "Save");
    assert_eq!(project.read("b.txt"), "new b\n");
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
}

/// Don't Save closes the tab and leaves the file as it was; Cancel keeps
/// the tab and its edits.
#[gpui::test]
fn closing_an_edited_tab_can_drop_the_edits_or_stay(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = with_files(cx, &project, &["a.txt", "b.txt"]);
    ws.set_auto_save(cx, AutoSave::Off);
    ws.type_text(cx, "new ");

    close_tab(ws, cx);
    answer(cx, "Cancel");
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);
    assert_eq!(ws.active_text(cx), "new b\n");
    assert!(ws.active_dirty(cx));
    assert_eq!(project.read("b.txt"), "b\n");

    close_tab(ws, cx);
    answer(cx, "Don't Save");
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(project.read("b.txt"), "b\n");
    // Opened again, it's the file's text.
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.active_text(cx), "b\n");
    assert!(!ws.active_dirty(cx));
}

/// With auto-save on (the default), closing an edited tab saves it rather
/// than asking.
#[gpui::test]
fn closing_an_edited_tab_with_auto_save_saves_it(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "new ");
    close_tab(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "new a\n");
    assert_eq!(ws.tabs(cx), [Vec::<String>::new()]);
}

/// With auto-save on (the default), an edit is written once typing has
/// paused for the delay, and not before.
#[gpui::test]
fn auto_save_writes_after_typing_pauses(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n")]);
    let ws = with_files(cx, &project, &["a.txt"]);
    ws.type_text(cx, "new ");
    wait(cx, 900);
    assert_eq!(project.read("a.txt"), "a\n");
    // Typing again starts the wait over.
    ws.type_text(cx, "and ");
    wait(cx, 900);
    assert_eq!(project.read("a.txt"), "a\n");
    wait(cx, 100);
    assert_eq!(project.read("a.txt"), "new and a\n");
    assert!(!ws.active_dirty(cx));
}

/// A clean tab closes without asking and the tab after it shows (the one
/// before it when it was the last).
#[gpui::test]
fn closing_a_clean_tab_shows_the_next_one(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    let ws = with_files(cx, &project, &["a.txt", "b.txt", "c.txt"]);
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));

    close_tab(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(ws.tabs(cx), [["a.txt", "c.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("c.txt")));
    assert_eq!(ws.active_text(cx), "c\n");

    close_tab(ws, cx);
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
}

/// Closing the last tab of a split closes the split; the other pane takes
/// its place and the keyboard.
#[gpui::test]
fn closing_the_last_tab_of_a_split_closes_it(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = with_files(cx, &project, &["a.txt", "b.txt"]);
    ws.act(cx, |ws, window, cx| ws.split_right(&SplitRight, window, cx));
    // The split shows the current file, in a pane of its own.
    assert_eq!(ws.tabs(cx), [vec!["a.txt", "b.txt"], vec!["b.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    // Edited in the split, the document is the other pane's too: closing
    // the split loses nothing, so it doesn't ask.
    ws.set_auto_save(cx, AutoSave::Off);
    ws.type_text(cx, "new ");

    close_tab(ws, cx);
    assert!(prompt(cx).is_none());
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_text(cx), "new b\n");
    assert!(ws.active_dirty(cx));
}
