//! The Test Explorer (Run ▸ Test Explorer): the project's tests as a tab of
//! the Find panel, by file, before anything runs, found the way the gutter's
//! ▶s are (`crate::test_explorer`). Every row has Run and, where it can be
//! debugged, Debug: a test, a group (a module, a class, a `describe`) or a
//! whole file. Test runs mark what they ran (✓ / ✕), from any Run tab. Like
//! the TODO list, the project is read the first time it's asked for, then
//! only files that change on disk or in a tab.

use std::{
    collections::BTreeMap,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

use super::*;
use crate::{
    results_panel::{Leading, ResultNode, ResultTarget, RowAction, TabButton},
    test_explorer::{self, TestItem},
    test_run::{Framework, Statuses},
};

/// Finds the Test Explorer's tab in the Find panel again.
pub(super) const TESTS_TAB: &str = "tests";
/// How long after a change (typing, a save, a checkout) files are read again.
const RESCAN_DELAY: Duration = Duration::from_millis(400);
/// Up to this many files, they show open.
const OPEN_FILES: usize = 12;

#[derive(Default)]
pub(super) struct TestExplorer {
    /// Each file's tests (files without any left out), once the first read
    /// of the project is done; None until the explorer is first asked for.
    files: Option<BTreeMap<PathBuf, Arc<Vec<TestItem>>>>,
    /// The whole project is being read.
    scanning: bool,
    /// Asked for before the project's files were listed: read them once they are.
    waiting_for_files: bool,
    /// Files (or folders) to read again.
    pending: HashSet<PathBuf>,
    /// The pause before reading the pending files.
    timer: Option<Task<()>>,
    /// Reading files (the project, or the pending ones).
    task: Option<Task<()>>,
    cancel: Arc<AtomicBool>,
}

impl TestExplorer {
    /// Kept up to date (the explorer was asked for in this project).
    fn active(&self) -> bool {
        self.files.is_some() || self.scanning
    }
}

impl Workspace {
    /// Test Explorer: the tests in the Find panel, reading the project the first time.
    pub(super) fn show_test_explorer(&mut self, _: &ShowTestExplorer, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tests.active() {
            self.scan_tests(cx);
        }
        let (title, nodes, buttons) = self.tests_tab(cx);
        self.results.update(cx, |p, cx| p.show_kind(TESTS_TAB, title, nodes, buttons, cx));
        self.has_results = true;
        self.bottom = Some(BottomPanel::Results);
        window.focus(&self.results.focus_handle(cx));
        cx.notify();
    }

    /// Another project: the explorer starts over when it's asked for.
    pub(super) fn reset_tests(&mut self) {
        self.tests.cancel.store(true, Ordering::Relaxed);
        self.tests = TestExplorer::default();
    }

    /// Reads every file of the project that may hold tests, in the background.
    fn scan_tests(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        self.tests.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.tests.cancel = cancel.clone();
        self.tests.pending.clear();
        let paths = self.project.searchable();
        // The project's files aren't listed yet: read them when they are.
        self.tests.waiting_for_files = paths.is_empty() && self.project.root() != root.as_path();
        let overrides = self.open_buffers(cx);
        self.tests.scanning = true;
        self.tests.task = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { test_explorer::scan(&paths, &overrides, &cancel) }).await;
            this.update(cx, |ws, cx| {
                ws.tests.scanning = false;
                ws.tests.files = Some(found.into_iter().filter(|(_, t)| !t.is_empty()).map(|(p, t)| (p, Arc::new(t))).collect());
                ws.tests.task = None;
                ws.refresh_tests_tab(cx);
                // Changes that came while it read.
                if !ws.tests.pending.is_empty() {
                    ws.schedule_tests_rescan(cx);
                }
            })
            .ok();
        }));
        self.refresh_tests_tab(cx);
    }

    /// The project's files were listed (or listed again).
    pub(super) fn tests_project_listed(&mut self, cx: &mut Context<Self>) {
        if self.tests.waiting_for_files {
            self.scan_tests(cx);
        }
    }

    /// Files (or folders) changed on disk or in a tab: read them again soon.
    pub(super) fn tests_files_changed(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        if !self.tests.active() {
            return;
        }
        let Some(root) = self.root.clone() else { return };
        self.tests.pending.extend(paths.into_iter().filter(|p| p.starts_with(&root)));
        if !self.tests.scanning && !self.tests.pending.is_empty() {
            self.schedule_tests_rescan(cx);
        }
    }

    /// A tab's text changed: its file's tests follow its unsaved text.
    pub(super) fn tests_edited(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if !self.tests.active() {
            return;
        }
        if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
            self.tests_files_changed([path], cx);
        }
    }

    /// Reads the pending files again after a pause (one more change restarts it).
    fn schedule_tests_rescan(&mut self, cx: &mut Context<Self>) {
        self.tests.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESCAN_DELAY).await;
            this.update(cx, |ws, cx| ws.rescan_tests(cx)).ok();
        }));
    }

    fn rescan_tests(&mut self, cx: &mut Context<Self>) {
        self.tests.timer = None;
        // A read is underway: it asks again for what's pending when it's done.
        if self.tests.task.is_some() {
            return;
        }
        let pending = std::mem::take(&mut self.tests.pending);
        let Some(files) = self.tests.files.as_mut() else { return };
        // A folder that went (or came, renamed): what was listed under it
        // goes, and what the project has in it now is read.
        let mut wanted = Vec::new();
        let openable = self.project.openable();
        for path in pending {
            files.retain(|p, _| p == &path || !p.starts_with(&path));
            if path.is_dir_on_host() {
                wanted.extend(openable.iter().filter(|p| p.starts_with(&path)).cloned());
            } else {
                wanted.push(path);
            }
        }
        // Not files Find in Files skips (ignored ones), nor ones that went.
        let (project, ignore) = (self.project.clone(), self.ignore.clone());
        wanted.retain(|path| {
            let listed = project.is_searchable(path);
            let unlisted = !project.lists(path) && path.is_file_on_host() && !ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore());
            let keep = (listed || unlisted) && test_explorer::may_hold_tests(path);
            if !keep {
                files.remove(path);
            }
            keep
        });
        if wanted.is_empty() {
            self.refresh_tests_tab(cx);
            return;
        }
        let overrides = self.unsaved_texts(&wanted, cx);
        let cancel = self.tests.cancel.clone();
        self.tests.task = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { test_explorer::scan(&wanted, &overrides, &cancel) }).await;
            this.update(cx, |ws, cx| {
                ws.tests.task = None;
                let Some(files) = ws.tests.files.as_mut() else { return };
                for (path, tests) in found {
                    if tests.is_empty() {
                        files.remove(&path);
                    } else {
                        files.insert(path, Arc::new(tests));
                    }
                }
                ws.refresh_tests_tab(cx);
                if !ws.tests.pending.is_empty() {
                    ws.schedule_tests_rescan(cx);
                }
            })
            .ok();
        }));
    }

    /// Updates the Test Explorer's tab, if it's open (test results came,
    /// files were read again).
    pub(super) fn refresh_tests_tab(&mut self, cx: &mut Context<Self>) {
        if !self.tests.active() {
            return;
        }
        let (title, nodes, buttons) = self.tests_tab(cx);
        self.results.update(cx, |p, cx| p.update_kind(TESTS_TAB, title, nodes, buttons, cx));
    }

    /// The tab's title, tree and buttons.
    fn tests_tab(&self, cx: &mut Context<Self>) -> (SharedString, Vec<ResultNode>, Vec<TabButton>) {
        let this = cx.entity().downgrade();
        let launch = Launch { this: this.clone() };
        let empty = BTreeMap::new();
        let files = self.tests.files.as_ref().unwrap_or(&empty);
        let statuses = &self.test_statuses;
        let open = files.len() <= OPEN_FILES;
        let mut nodes: Vec<ResultNode> =
            files.iter().map(|(path, items)| file_node(path, items, &self.location_label(path), open, statuses, &launch)).collect();
        if nodes.is_empty() {
            nodes.push(ResultNode::message(if self.tests.scanning { "Looking for tests…" } else { "No tests found in the project" }));
        }
        let mut buttons = Vec::new();
        // What the project runs all its tests with (cargo test, go test ./..., pytest).
        let all = self.root.as_deref().and_then(|root| crate::run::project_targets(root, None).into_iter().find(|t| Framework::of(&t.command).is_some()));
        if let Some(all) = all {
            let this = this.clone();
            buttons.push(TabButton {
                icon: Icon::Play,
                label: "Run All".into(),
                on: false,
                on_click: Rc::new(move |window, cx| {
                    this.update(cx, |ws, cx| ws.run_target(all.clone(), window, cx)).ok();
                }),
            });
        }
        buttons.push(TabButton {
            icon: Icon::Refresh,
            label: "Refresh".into(),
            on: false,
            on_click: Rc::new(move |_, cx| {
                this.update(cx, |ws, cx| ws.scan_tests(cx)).ok();
            }),
        });
        ("Tests".into(), nodes, buttons)
    }
}

/// Makes rows' Run and Debug buttons.
struct Launch {
    this: gpui::WeakEntity<Workspace>,
}

impl Launch {
    fn actions(&self, target: &RunTarget) -> Vec<RowAction> {
        let mut actions = Vec::new();
        let (this, run) = (self.this.clone(), target.clone());
        actions.push(RowAction {
            icon: Icon::Play,
            tooltip: format!("Run {}", target.name.trim_start_matches("Run ")).into(),
            run: Rc::new(move |window, cx| {
                this.update(cx, |ws, cx| ws.run_target(run.clone(), window, cx)).ok();
            }),
        });
        if target.debug.is_some() {
            let (this, debug) = (self.this.clone(), target.clone());
            actions.push(RowAction {
                icon: Icon::Bug,
                tooltip: format!("Debug {}", target.name.trim_start_matches("Run ")).into(),
                run: Rc::new(move |window, cx| {
                    this.update(cx, |ws, cx| ws.debug_target(debug.clone(), window, cx)).ok();
                }),
            });
        }
        actions
    }
}

fn plural_tests(n: usize) -> String {
    format!("{n} test{}", if n == 1 { "" } else { "s" })
}

/// A file's row: its tests, and Run / Debug for all of them when its
/// runner can say just this file's.
fn file_node(path: &Path, items: &[TestItem], shown: &str, open: bool, statuses: &Statuses, launch: &Launch) -> ResultNode {
    let (folder, name) = match shown.rsplit_once('/') {
        Some((folder, name)) => (Some(folder.to_string()), name.to_string()),
        None => (None, shown.to_string()),
    };
    let count = plural_tests(items.iter().map(TestItem::count).sum());
    ResultNode {
        leading: Leading::File(path.to_path_buf()),
        label: name.into(),
        highlight: None,
        detail: Some(match folder {
            Some(folder) => format!("{folder}  ·  {count}").into(),
            None => count.into(),
        }),
        target: None,
        open: None,
        children: items.iter().map(|item| item_node(path, item, statuses, launch)).collect(),
        loader: None,
        expanded: open,
        actions: test_explorer::file_target(path, items).map(|t| launch.actions(&t)).unwrap_or_default(),
    }
}

/// A test's or group's row: its last result's mark, going to where it's written.
fn item_node(path: &Path, item: &TestItem, statuses: &Statuses, launch: &Launch) -> ResultNode {
    let group = !item.children.is_empty();
    let leading = match item.status(statuses) {
        Some(status) => {
            let (icon, tone) = status.icon();
            Leading::Tinted(icon, tone)
        }
        None => Leading::Icon(if group { Icon::Braces } else { Icon::Function }),
    };
    let line = item.target.row.unwrap_or(item.target.rows.start);
    ResultNode {
        leading,
        label: item.label().to_string().into(),
        highlight: None,
        detail: group.then(|| plural_tests(item.count()).into()),
        target: Some(ResultTarget { path: path.to_path_buf(), position: Position { line: line as u32, character: 0 }, encoding: Encoding::Utf8 }),
        open: None,
        children: item.children.iter().map(|c| item_node(path, c, statuses, launch)).collect(),
        loader: None,
        expanded: true,
        actions: launch.actions(&item.target),
    }
}
