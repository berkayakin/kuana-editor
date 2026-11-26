//! Hot exit: a project's tabs and unsaved text kept in its session when the
//! window closes, and brought back by the next window that opens it.

use gpui::TestAppContext;

use super::harness::{self, TempProject, Ws, prompt, settle, wait};
use super::*;

/// A window with the project open, auto-save off (with it on, edits reach
/// the files and there's little unsaved to keep).
fn open(cx: &mut TestAppContext, project: &TempProject) -> Ws {
    let ws = harness::workspace(cx);
    ws.set_auto_save(cx, AutoSave::Off);
    ws.open_project(cx, &project.dir);
    ws
}

/// Closes the window as its close button does: the project's session is
/// written (unsaved text included) and nothing is asked.
fn close(ws: Ws, cx: &mut TestAppContext) {
    let closes = ws.update(cx, |ws, window, cx| ws.prepare_to_close(CloseIntent::CloseWindow, window, cx));
    assert!(closes, "a project's window closes without asking");
    assert!(prompt(cx).is_none());
    ws.update(cx, |_, window, _| window.remove_window());
    settle(cx);
}

fn edit(ws: Ws, cx: &mut TestAppContext, path: &Path, at: usize, text: &str) {
    let editor = ws.editor_for(cx, path).unwrap();
    editor.update(cx, |e, cx| e.replace_ranges(vec![(at..at, text.into())], cx));
}

fn dirty(ws: Ws, cx: &mut TestAppContext, path: &Path) -> bool {
    let editor = ws.editor_for(cx, path).unwrap();
    editor.read_with(cx, |e, cx| e.doc.read(cx).is_dirty())
}

fn loaded(ws: Ws, cx: &mut TestAppContext, path: &Path) -> bool {
    let editor = ws.editor_for(cx, path).unwrap();
    editor.read_with(cx, |e, _| e.is_loaded())
}

/// The tabs come back in their order with the same one current, its
/// unsaved text back and unsaved, and each tab's caret where it was.
#[gpui::test]
fn a_closed_project_comes_back_with_its_tabs_and_unsaved_text(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "alpha\nbeta\n"), ("b.txt", "one\ntwo\nthree\n"), ("c.txt", "x\ny\nz\n")]);
    let ws = open(cx, &project);
    for name in ["a.txt", "b.txt", "c.txt"] {
        ws.open(cx, &project.path(name));
    }
    // c: the caret on its last line.
    ws.put_caret(cx, 4);
    // a: the caret in its second line.
    ws.open(cx, &project.path("a.txt"));
    ws.put_caret(cx, 8);
    // b, current: edited, the caret after the edit.
    ws.open(cx, &project.path("b.txt"));
    ws.put_caret(cx, 4);
    ws.type_text(cx, "one and a half\n");
    assert_eq!(ws.active_caret(cx), (3, 1));
    close(ws, cx);
    // The edit wasn't written.
    assert_eq!(project.read("b.txt"), "one\ntwo\nthree\n");

    let ws = open(cx, &project);
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt", "c.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_text(cx), "one\none and a half\ntwo\nthree\n");
    assert!(ws.active_dirty(cx));
    assert_eq!(ws.active_caret(cx), (3, 1));
    // The others read their files when shown, carets as they were.
    ws.open(cx, &project.path("a.txt"));
    assert_eq!(ws.active_text(cx), "alpha\nbeta\n");
    assert!(!ws.active_dirty(cx));
    assert_eq!(ws.active_caret(cx), (2, 3));
    ws.open(cx, &project.path("c.txt"));
    assert_eq!(ws.active_caret(cx), (3, 1));
    // Undo goes back to the file's text: the restored edit is one on top of it.
    ws.open(cx, &project.path("b.txt"));
    let editor = ws.active_editor(cx);
    editor.update(cx, |e, cx| e.undo_step(cx));
    assert_eq!(ws.active_text(cx), "one\ntwo\nthree\n");
}

/// The session is also written a moment after each edit, so unsaved text
/// survives a crash (no window close to write it): a window opened on the
/// project afterwards has it.
#[gpui::test]
fn unsaved_text_is_kept_soon_after_typing(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n")]);
    let ws = open(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    // A session from opening the file, written before the edit.
    wait(cx, 1000);
    ws.type_text(cx, "new ");
    // Not yet: the save waits for typing to pause.
    wait(cx, 700);
    let session = crate::state::ProjectSession::load(&project.dir).unwrap();
    assert_eq!(session.tabs[0].unsaved, None);
    wait(cx, 100);
    let session = crate::state::ProjectSession::load(&project.dir).unwrap();
    assert_eq!(session.tabs[0].unsaved.as_deref(), Some("new a\n"));

    // The first window never closed.
    let again = open(cx, &project);
    assert_eq!(again.active_text(cx), "new a\n");
    assert!(again.active_dirty(cx));
    assert_eq!(project.read("a.txt"), "a\n");
}

/// Unsaved text the file has by now (saved after the session was written)
/// comes back as the file, not as an edit.
#[gpui::test]
fn unsaved_text_the_file_has_by_now_comes_back_clean(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = open(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    ws.open(cx, &project.path("b.txt"));
    edit(ws, cx, &project.path("a.txt"), 0, "new ");
    edit(ws, cx, &project.path("b.txt"), 0, "new ");
    close(ws, cx);
    // Another program wrote both files: a with the text the session kept, b
    // with something else.
    project.write("a.txt", "new a\n");
    project.write("b.txt", "other b\n");

    let ws = open(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    assert_eq!(ws.active_text(cx), "new a\n");
    assert!(!ws.active_dirty(cx));
    // b's kept text differs from the file's: still unsaved, over the file.
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.active_text(cx), "new b\n");
    assert!(ws.active_dirty(cx));
}

/// A tab whose file was deleted since: gone when it had nothing unsaved,
/// kept with its text when it had.
#[gpui::test]
fn a_restored_tab_whose_file_is_gone(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    let ws = open(cx, &project);
    for name in ["a.txt", "b.txt", "c.txt"] {
        ws.open(cx, &project.path(name));
    }
    edit(ws, cx, &project.path("b.txt"), 0, "new ");
    close(ws, cx);
    std::fs::remove_file(project.path("a.txt")).unwrap();
    std::fs::remove_file(project.path("b.txt")).unwrap();

    let ws = open(cx, &project);
    assert_eq!(ws.tabs(cx), [["b.txt", "c.txt"]]);
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.tabs(cx), [["b.txt", "c.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_text(cx), "new b\n");
    assert!(ws.active_dirty(cx));
    // Still the file's tab the time after.
    close(ws, cx);
    let ws = open(cx, &project);
    assert_eq!(ws.tabs(cx), [["b.txt", "c.txt"]]);
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.active_text(cx), "new b\n");
    // Saved, the file is back. (Unlike a file deleted while its tab was
    // open, nothing asks: the restored text was never the file's.)
    ws.act(cx, |ws, window, cx| ws.save(&Save, window, cx));
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("b.txt"), "new b\n");
    assert!(!ws.active_dirty(cx));
}

/// A file deleted after it was shown and before the window closed, with
/// nothing unsaved: its tab doesn't come back, and the pane it was alone in
/// neither.
#[gpui::test]
fn a_pane_whose_tabs_are_all_gone_is_left_out(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = open(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    ws.act(cx, |ws, window, cx| ws.split_right(&SplitRight, window, cx));
    ws.open(cx, &project.path("b.txt"));
    assert_eq!(ws.tabs(cx), [vec!["a.txt"], vec!["a.txt", "b.txt"]]);
    close(ws, cx);
    std::fs::remove_file(project.path("a.txt")).unwrap();

    let ws = open(cx, &project);
    assert_eq!(ws.tabs(cx), [["b.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_text(cx), "b\n");
}

/// Save All right after the project comes back, before its tabs were
/// shown: each tab's file is read first, then written with the session's
/// unsaved text (not with the empty stand-in it shows until it's read).
#[gpui::test]
fn save_all_of_restored_tabs_never_shown_keeps_their_text(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    let ws = open(cx, &project);
    for name in ["a.txt", "b.txt", "c.txt"] {
        ws.open(cx, &project.path(name));
    }
    edit(ws, cx, &project.path("a.txt"), 0, "new ");
    edit(ws, cx, &project.path("b.txt"), 0, "new ");
    close(ws, cx);

    let ws = open(cx, &project);
    assert!(!loaded(ws, cx, &project.path("a.txt")));
    assert!(!loaded(ws, cx, &project.path("b.txt")));
    assert!(dirty(ws, cx, &project.path("a.txt")));
    ws.act(cx, |ws, _, cx| ws.save_tabs(cx));
    assert_eq!(project.read("a.txt"), "new a\n");
    assert_eq!(project.read("b.txt"), "new b\n");
    assert_eq!(project.read("c.txt"), "c\n");
    assert!(!dirty(ws, cx, &project.path("a.txt")));
    assert!(!dirty(ws, cx, &project.path("b.txt")));
    ws.open(cx, &project.path("a.txt"));
    assert_eq!(ws.active_text(cx), "new a\n");
}

/// Save in the current restored tab before its file was read (the read is
/// in the background): the file is read now and the session's text written;
/// the background read arriving afterwards changes nothing.
#[gpui::test]
fn save_of_a_restored_tab_before_its_read_lands(cx: &mut TestAppContext) {
    harness::init(cx);
    let project = TempProject::new(&[("a.txt", "a\n")]);
    let ws = open(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    edit(ws, cx, &project.path("a.txt"), 0, "new ");
    close(ws, cx);

    // Opened without settling: the current tab's read is under way.
    let ws = harness::workspace(cx);
    ws.set_auto_save(cx, AutoSave::Off);
    let dir = project.dir.clone();
    ws.update(cx, |ws, window, cx| {
        ws.set_root(dir, window, cx);
        assert!(!ws.active_editor(cx).unwrap().read(cx).is_loaded());
        ws.save(&Save, window, cx);
    });
    settle(cx);
    assert!(prompt(cx).is_none());
    assert_eq!(project.read("a.txt"), "new a\n");
    assert_eq!(ws.active_text(cx), "new a\n");
    assert!(!ws.active_dirty(cx));
}
