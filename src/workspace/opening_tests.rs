//! Opening files in tabs (`opening.rs`): asked for on the UI thread, looked
//! at and read in the background, shown in the order asked.

use gpui::{EntityInputHandler, TestAppContext};

use super::harness::{self, TempProject, Ws, answer, prompt, settle};
use super::*;

fn with_project(cx: &mut TestAppContext, project: &TempProject) -> Ws {
    harness::init(cx);
    let ws = harness::workspace(cx);
    ws.open_project(cx, &project.dir);
    ws
}

/// Files asked for one after another get their tabs in that order, the
/// last one current, whichever read finishes first. Each iteration runs the
/// executor's ready tasks in another (seeded) order, so the reads finish in
/// different orders (a file's size makes no difference there).
#[gpui::test(iterations = 25)]
fn files_asked_for_in_a_row_get_tabs_in_that_order(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    let ws = with_project(cx, &project);
    for name in ["a.txt", "b.txt", "c.txt"] {
        ws.open_soon(cx, &project.path(name));
    }
    // Nothing shows before its file is read.
    assert_eq!(ws.tabs(cx), [Vec::<String>::new()]);
    settle(cx);
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt", "c.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("c.txt")));
    assert_eq!(ws.active_text(cx), "c\n");
    assert_eq!(ws.read(cx, |ws, _| ws.opens), 0);
}

/// A file in a tab already is shown, not opened again, also when asked
/// for again while a read is under way.
#[gpui::test]
fn a_file_in_a_tab_is_shown_not_opened_twice(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let ws = with_project(cx, &project);
    // Else switching tabs would save the edit.
    ws.set_auto_save(cx, AutoSave::Off);
    ws.open(cx, &project.path("a.txt"));
    ws.open(cx, &project.path("b.txt"));
    ws.type_text(cx, "edited ");

    // Shown at once: no read.
    ws.open_soon(cx, &project.path("a.txt"));
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    settle(cx);
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);

    // Asked for twice in a row before either was read: one tab.
    for _ in 0..2 {
        ws.open_soon(cx, &project.path("b.txt"));
    }
    settle(cx);
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    // The tab it had, unsaved edits and all.
    assert_eq!(ws.active_text(cx), "edited b\n");
    assert!(ws.active_dirty(cx));
}

/// The same file by another name (a link to it) ends in its one tab.
#[cfg(unix)]
#[gpui::test]
fn a_file_asked_for_by_a_link_is_the_same_tab(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("real.txt", "text\n")]);
    std::os::unix::fs::symlink(project.path("real.txt"), project.path("link.txt")).unwrap();
    let ws = with_project(cx, &project);

    // Asked for by the link first: the tab is the file's, under its real path.
    ws.open(cx, &project.path("link.txt"));
    assert_eq!(ws.tabs(cx), [["real.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("real.txt")));
    ws.type_text(cx, "more ");

    ws.open(cx, &project.path("real.txt"));
    ws.open(cx, &project.path("link.txt"));
    assert_eq!(ws.tabs(cx), [["real.txt"]]);
    assert_eq!(ws.active_text(cx), "more text\n");

    // Both names asked for together before either is read.
    let ws2 = harness::workspace(cx);
    ws2.open_project(cx, &project.dir);
    ws2.open_soon(cx, &project.path("link.txt"));
    ws2.open_soon(cx, &project.path("real.txt"));
    settle(cx);
    assert_eq!(ws2.tabs(cx), [["real.txt"]]);
}

/// `open_at` (a `file:line:column` link) puts the caret there, 1-based,
/// columns in characters.
#[gpui::test]
fn open_at_lands_on_the_line_and_column(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "first\nsécond line\nthird\n")]);
    let ws = with_project(cx, &project);
    let path = project.path("a.txt");
    ws.act(cx, |ws, window, cx| ws.open_at(path.clone(), Some(2), Some(4), window, cx));
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    assert_eq!(ws.active_caret(cx), (2, 4));
    // Already open: the same, at once.
    ws.update(cx, |ws, window, cx| ws.open_at(path.clone(), Some(3), None, window, cx));
    assert_eq!(ws.active_caret(cx), (3, 1));
    // A line past the end: the last one.
    ws.act(cx, |ws, window, cx| ws.open_at(path.clone(), Some(99), Some(1), window, cx));
    assert_eq!(ws.active_caret(cx), (4, 1));
}

/// A jump into another file records the place it left: Back returns there.
#[gpui::test]
fn back_returns_to_where_a_jump_left(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "one\ntwo\nthree\n"), ("b.txt", "b1\nb2\nb3\n")]);
    let ws = with_project(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    ws.put_caret(cx, 9);
    assert_eq!(ws.active_caret(cx), (3, 2));

    let b = project.path("b.txt");
    ws.act(cx, |ws, window, cx| ws.open_at(b, Some(2), Some(2), window, cx));
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_caret(cx), (2, 2));

    ws.act(cx, |ws, window, cx| ws.navigate_back(&NavigateBack, window, cx));
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    assert_eq!(ws.active_caret(cx), (3, 2));
    ws.act(cx, |ws, window, cx| ws.navigate_forward(&NavigateForward, window, cx));
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    assert_eq!(ws.active_caret(cx), (2, 2));
}

/// Restores `project` with `a.txt` current and `b.txt` not shown yet
/// (unsaved text in it when `b_unsaved`).
fn restored(cx: &mut TestAppContext, project: &TempProject, b_unsaved: bool) -> Ws {
    harness::init(cx);
    let ws = harness::workspace(cx);
    ws.set_auto_save(cx, AutoSave::Off);
    ws.open_project(cx, &project.dir);
    ws.open(cx, &project.path("b.txt"));
    if b_unsaved {
        ws.type_text(cx, "new ");
    }
    ws.open(cx, &project.path("a.txt"));
    let closes = ws.update(cx, |ws, window, cx| ws.prepare_to_close(CloseIntent::CloseWindow, window, cx));
    assert!(closes);
    ws.update(cx, |_, window, _| window.remove_window());
    settle(cx);

    let ws = harness::workspace(cx);
    ws.set_auto_save(cx, AutoSave::Off);
    ws.open_project(cx, &project.dir);
    assert_eq!(ws.tabs(cx), [["b.txt", "a.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    let b = ws.editor_for(cx, &project.path("b.txt")).unwrap();
    assert!(!b.read_with(cx, |e, _| e.is_loaded()));
    ws
}

/// A jump into a restored tab whose file isn't read yet waits for the
/// text, then lands; Back returns to where it left.
#[gpui::test]
fn a_jump_into_a_restored_tab_waits_for_its_text(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a1\na2\na3\n"), ("b.txt", "one\ntwo\nthree\n")]);
    let ws = restored(cx, &project, false);
    ws.put_caret(cx, 3);
    let b = project.path("b.txt");
    ws.update(cx, |ws, window, cx| ws.open_at(b, Some(3), Some(3), window, cx));
    // The tab shows at once, empty until its file is read.
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    settle(cx);
    assert_eq!(ws.active_text(cx), "one\ntwo\nthree\n");
    assert_eq!(ws.active_caret(cx), (3, 3));

    ws.act(cx, |ws, window, cx| ws.navigate_back(&NavigateBack, window, cx));
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    assert_eq!(ws.active_caret(cx), (2, 1));
}

/// The same with unsaved text kept for the tab: the jump lands in that text.
#[gpui::test]
fn a_jump_into_a_restored_tab_lands_in_its_unsaved_text(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "one\ntwo\nthree\n")]);
    let ws = restored(cx, &project, true);
    let b = project.path("b.txt");
    ws.update(cx, |ws, window, cx| ws.open_at(b, Some(1), Some(6), window, cx));
    settle(cx);
    assert_eq!(ws.active_text(cx), "new one\ntwo\nthree\n");
    assert!(ws.active_dirty(cx));
    assert_eq!(ws.active_caret(cx), (1, 6));
}

/// Edits made to a restored tab before its text is there, by code that
/// reaches its editor directly (keys don't reach it: until then the tab
/// draws a placeholder, without the editor element that takes them),
/// change nothing, and the next frame draws without reading past the empty
/// stand-in; the text comes as the file has it.
#[gpui::test]
fn direct_edits_to_a_restored_tab_before_its_text_change_nothing(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("b.txt", "one\ntwo\n")]);
    let ws = restored(cx, &project, false);
    ws.open_soon(cx, &project.path("b.txt"));
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
    let editor = ws.active_editor(cx);
    assert!(!editor.read_with(cx, |e, _| e.is_loaded()));
    // Each update ends with a frame, which drew the status bar's count of
    // the selected text where the carets were.
    ws.update(cx, |_, window, cx| editor.update(cx, |e, cx| e.replace_text_in_range(None, "typed", window, cx)));
    editor.update(cx, |e, cx| e.replace_ranges(vec![(0..0, "replaced".into())], cx));
    ws.update(cx, |_, _, cx| cx.notify());
    assert!(!ws.active_dirty(cx));
    settle(cx);
    assert_eq!(ws.active_text(cx), "one\ntwo\n");
    assert!(!ws.active_dirty(cx));
    assert_eq!(project.read("b.txt"), "one\ntwo\n");
}

/// The harness's project is trusted: no dialog over the workspace. One
/// opened without trusting it is in Restricted Mode, the trust dialog asking.
#[gpui::test]
fn an_untrusted_project_asks_to_be_trusted(cx: &mut TestAppContext) {
    let trusted = TempProject::new(&[("a.txt", "a\n")]);
    let ws = with_project(cx, &trusted);
    assert!(ws.read(cx, |ws, cx| ws.modals.current.is_none() && !ws.restricted(cx)));

    let untrusted = TempProject::new(&[("a.txt", "a\n")]);
    let ws = harness::workspace(cx);
    ws.open_project_restricted(cx, &untrusted.dir);
    assert!(ws.read(cx, |ws, cx| matches!(ws.modals.current, Some(Modal::Trust(_))) && ws.restricted(cx)));
}

/// A folder opens as the project, in this window; no tab.
#[gpui::test]
fn a_folder_opens_as_the_project(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n"), ("sub/s.txt", "s\n")]);
    let ws = with_project(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    ws.open(cx, &project.path("sub"));
    assert!(prompt(cx).is_none());
    assert_eq!(ws.read(cx, |ws, _| ws.primary_folder().map(Path::to_path_buf)), Some(project.path("sub")));
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
}

/// A file that isn't there: an error says so, and no tab is left behind.
#[gpui::test]
fn a_missing_file_says_so_without_a_tab(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n")]);
    let ws = with_project(cx, &project);
    ws.open(cx, &project.path("a.txt"));
    ws.open(cx, &project.path("missing.txt"));
    let (message, _) = prompt(cx).expect("an error");
    assert_eq!(message, format!("Couldn't open {}", project.path("missing.txt").display()));
    // GPUI's own name for an OK button.
    answer(cx, "Ok");
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));
    // A file asked for right after it still opens, in its turn.
    project.write("b.txt", "b\n");
    ws.open_soon(cx, &project.path("missing.txt"));
    ws.open_soon(cx, &project.path("b.txt"));
    settle(cx);
    answer(cx, "Ok");
    assert!(prompt(cx).is_none());
    assert_eq!(ws.tabs(cx), [["a.txt", "b.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.txt")));
}

/// A binary file asks first: Cancel opens nothing, Open Anyway opens it
/// as text.
#[gpui::test]
fn a_binary_file_asks_before_opening(cx: &mut TestAppContext) {
    let project = TempProject::new(&[("a.txt", "a\n")]);
    std::fs::write(project.path("b.bin"), [0u8, 159, 146, 150, 0, b'h', b'i']).unwrap();
    let ws = with_project(cx, &project);
    ws.open(cx, &project.path("a.txt"));

    ws.open(cx, &project.path("b.bin"));
    let (message, _) = prompt(cx).expect("asks first");
    assert_eq!(message, "“b.bin” looks like a binary file");
    answer(cx, "Cancel");
    assert_eq!(ws.tabs(cx), [["a.txt"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("a.txt")));

    ws.open(cx, &project.path("b.bin"));
    answer(cx, "Open Anyway");
    assert!(prompt(cx).is_none());
    assert_eq!(ws.tabs(cx), [["a.txt", "b.bin"]]);
    assert_eq!(ws.active_path(cx), Some(project.path("b.bin")));
    assert!(ws.active_text(cx).ends_with("hi"));
}
