//! The Test Explorer's model: the project's tests, found without running
//! anything. A test file is read and parsed like an open one and asked for
//! its run targets (`run::file_targets`, the gutter's ▶s); the ones naming a
//! test, and the groups around them (a `mod tests`, a test class, a
//! `describe`), become a tree by which lines hold which. The workspace side
//! (`workspace/test_explorer.rs`) shows it in the Find panel with each
//! node's Run and Debug, and the marks of the last runs.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use ropey::Rope;

use crate::{
    run::{self, RunTarget},
    syntax::{Language, Syntax},
    test_run::{self, Statuses, TestStatus},
};

/// A test, or a group of them, with what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct TestItem {
    pub target: RunTarget,
    pub children: Vec<TestItem>,
}

impl TestItem {
    /// The tests in it (itself, when it's one).
    pub fn count(&self) -> usize {
        if self.children.is_empty() { usize::from(self.target.test.is_some()) } else { self.children.iter().map(TestItem::count).sum() }
    }

    /// How its last runs went: its own result, else its tests' together.
    pub fn status(&self, statuses: &Statuses) -> Option<TestStatus> {
        if let Some(status) = self.target.test.as_ref().and_then(|test| test_run::status_of(test, statuses)) {
            return Some(status);
        }
        self.children.iter().filter_map(|c| c.status(statuses)).fold(None, |acc, s| Some(TestStatus::combine(acc, s)))
    }

    /// The test's or group's name, without the "Test '…'" around it.
    pub fn label(&self) -> &str {
        let name = self.target.name.as_str();
        name.strip_prefix("Tests in '").or_else(|| name.strip_prefix("Test '")).and_then(|n| n.strip_suffix('\'')).unwrap_or(name)
    }
}

/// Whether a file may hold tests, by its name and language: Rust files,
/// Go's `_test.go`, pytest's `test_*.py` / `*_test.py`, JavaScript and
/// TypeScript `*.test.*` / `*.spec.*` or under `__tests__`, and files of
/// extension languages that say what runs (`runnables.scm`).
pub fn may_hold_tests(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    match Language::from_path(path) {
        Language::Rust => true,
        Language::Go => name.ends_with("_test.go"),
        Language::Python => name.starts_with("test_") || name.ends_with("_test.py"),
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            name.contains(".test.") || name.contains(".spec.") || path.components().any(|c| c.as_os_str() == "__tests__")
        }
        language @ Language::Extension(_) => crate::syntax::queries(language).is_some_and(|q| q.runnables.is_some()),
        _ => false,
    }
}

/// A quick look before parsing: most Rust files have no tests.
fn worth_parsing(language: Language, text: &str) -> bool {
    match language {
        Language::Rust => text.contains("#[test") || text.contains("::test") || text.contains("#[rstest"),
        _ => text.contains("test") || text.contains("Test") || text.contains("it("),
    }
}

/// A file's tests and groups as a tree.
pub fn file_tests(path: &Path, text: &str) -> Vec<TestItem> {
    let language = Language::from_path(path);
    if !worth_parsing(language, text) {
        return Vec::new();
    }
    let syntax = Syntax::new(language, &Rope::from_str(text));
    let Some(tree) = syntax.tree() else { return Vec::new() };
    nest(run::file_targets(language, path, text, tree))
}

/// The targets naming a test, and those holding one, nested by the lines
/// they span (a group's rows hold its tests'). Others (Run 'main') go.
pub fn nest(targets: Vec<RunTarget>) -> Vec<TestItem> {
    let within = |inner: &RunTarget, outer: &RunTarget| inner.rows.start >= outer.rows.start && inner.rows.end <= outer.rows.end && inner.rows != outer.rows;
    let mut kept: Vec<RunTarget> = Vec::new();
    for target in &targets {
        let holds_test = targets.iter().any(|t| t.test.is_some() && within(t, target));
        // An extension's tasks may share a node: one entry per test.
        let repeat = kept.iter().any(|k| k.rows == target.rows && k.test == target.test);
        if (target.test.is_some() || holds_test) && !repeat {
            kept.push(target.clone());
        }
    }
    // Outer before inner: by first line, wider first.
    kept.sort_by(|a, b| a.rows.start.cmp(&b.rows.start).then(b.rows.end.cmp(&a.rows.end)));
    fn insert(items: &mut Vec<TestItem>, target: RunTarget) {
        if let Some(last) = items.last_mut()
            && target.rows.start >= last.target.rows.start
            && target.rows.end <= last.target.rows.end
            && target.rows != last.target.rows
        {
            insert(&mut last.children, target);
            return;
        }
        items.push(TestItem { target, children: Vec::new() });
    }
    let mut items = Vec::new();
    for target in kept {
        insert(&mut items, target);
    }
    // A group whose tests all went (no test key under it) isn't shown.
    fn prune(items: &mut Vec<TestItem>) {
        for item in items.iter_mut() {
            prune(&mut item.children);
        }
        items.retain(|i| i.count() > 0);
    }
    prune(&mut items);
    items
}

/// Every test in a file at once, for its row: `run::file_tests` over the
/// file's outermost items.
pub fn file_target(path: &Path, items: &[TestItem]) -> Option<RunTarget> {
    let roots: Vec<&RunTarget> = items.iter().map(|i| &i.target).collect();
    run::file_tests(path, &roots)
}

/// The files held by the project at most: past this many, the rest aren't read.
pub const MAX_FILES: usize = 20_000;

/// Each file's tests (none for a file that's gone, unreadable or without
/// any), on several threads; `overrides` are open files' unsaved text.
/// Stops between files once `cancel` is set.
pub fn scan(paths: &[PathBuf], overrides: &HashMap<PathBuf, Arc<str>>, cancel: &AtomicBool) -> Vec<(PathBuf, Vec<TestItem>)> {
    let paths: Vec<&PathBuf> = paths.iter().filter(|p| may_hold_tests(p)).take(MAX_FILES).collect();
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).clamp(1, paths.len().max(1));
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::with_capacity(paths.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !cancel.load(Ordering::Relaxed) {
                    let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    let tests = match overrides.get(*path) {
                        Some(text) => file_tests(path, text),
                        None => crate::todo::read(path).map(|text| file_tests(path, &text)).unwrap_or_default(),
                    };
                    found.lock().unwrap_or_else(|e| e.into_inner()).push(((*path).clone(), tests));
                }
            });
        }
    });
    found.into_inner().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_run::Framework;

    /// An empty folder of the test's own.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-test-explorer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shape(items: &[TestItem]) -> Vec<String> {
        fn walk(items: &[TestItem], depth: usize, out: &mut Vec<String>) {
            for item in items {
                out.push(format!("{}{}", "  ".repeat(depth), item.label()));
                walk(&item.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(items, 0, &mut out);
        out
    }

    #[test]
    fn a_rust_files_tests_nest_in_their_modules() {
        let dir = folder("rust");
        std::fs::write(dir.as_path().join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::create_dir(dir.as_path().join("src")).unwrap();
        std::fs::write(dir.as_path().join("src/lib.rs"), "mod parse;\n").unwrap();
        let path = dir.as_path().join("src/parse.rs");
        let text = "pub fn parse() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn reads() {}\n\n    mod errors {\n        #[test]\n        fn rejects() {}\n    }\n\n    fn helper() {}\n}\n";
        let items = file_tests(&path, text);
        assert_eq!(shape(&items), ["tests", "  reads", "  errors", "    rejects"]);
        assert_eq!(items[0].count(), 2);
        assert_eq!(items[0].children[1].children[0].target.test, Some((Framework::Cargo, "parse::tests::errors::rejects".into())));
        // The file's row runs its module's tests: one group, so that group.
        let file = file_target(&path, &items).unwrap();
        assert_eq!(file.command, items[0].target.command);
        // Two top-level tests: the module's name as the filter.
        let two = "#[test]\nfn a() {}\n#[test]\nfn b() {}\n";
        let items = file_tests(&path, two);
        assert_eq!(shape(&items), ["a", "b"]);
        let file = file_target(&path, &items).unwrap();
        assert_eq!(file.command, "cargo test -- parse::");
        assert_eq!(file.name, "Tests in 'parse.rs'");
        // No tests, nothing parsed.
        assert!(file_tests(&path, "fn main() {}\n").is_empty());
    }

    #[test]
    fn python_and_javascript_groups_hold_their_tests() {
        let dir = folder("groups");
        let py = dir.as_path().join("test_math.py");
        let items = file_tests(&py, "import pytest\n\nclass TestAdd:\n    def test_small(self):\n        pass\n\n    def helper(self):\n        pass\n\ndef test_top():\n    pass\n\nif __name__ == '__main__':\n    pass\n");
        assert_eq!(shape(&items), ["TestAdd", "  test_small", "test_top"]);
        let file = file_target(&py, &items).unwrap();
        assert!(file.command.ends_with(&format!("-m pytest -v {}", run::shell_quote(&py.to_string_lossy()))), "{}", file.command);

        let js = dir.as_path().join("sum.test.js");
        let items = file_tests(&js, "describe('sum', () => {\n  it('adds', () => {});\n  describe('empty', () => {\n    test('is zero', () => {});\n  });\n});\ntest('alone', () => {});\n");
        assert_eq!(shape(&items), ["sum", "  adds", "  empty", "    is zero", "alone"]);
        assert_eq!(items[0].count(), 2);
        let file = file_target(&js, &items).unwrap();
        assert!(file.command.starts_with("node --test "), "{}", file.command);
    }

    #[test]
    fn statuses_mark_tests_and_their_groups() {
        let dir = folder("statuses");
        let go = dir.as_path().join("sum_test.go");
        let items = file_tests(&go, "package sum\n\nfunc TestAdd(t *testing.T) {}\n\nfunc TestSub(t *testing.T) {}\n\nfunc helper() {}\n");
        assert_eq!(shape(&items), ["TestAdd", "TestSub"]);
        let mut statuses = Statuses::new();
        statuses.insert((Framework::Go, "TestAdd".into()), TestStatus::Passed);
        assert_eq!(items[0].status(&statuses), Some(TestStatus::Passed));
        assert_eq!(items[1].status(&statuses), None);
        let group = TestItem { target: RunTarget { name: "Tests in 'x'".into(), ..items[0].target.clone() }, children: items.clone() };
        statuses.insert((Framework::Go, "TestSub".into()), TestStatus::Failed);
        assert_eq!(TestItem { target: RunTarget { test: None, ..group.target.clone() }, ..group }.status(&statuses), Some(TestStatus::Failed));
        let file = file_target(&go, &items).unwrap();
        assert_eq!(file.command, "go test -v -run '^(TestAdd|TestSub)$' .");
    }

    #[test]
    fn which_files_are_read() {
        for (path, yes) in [
            ("src/lib.rs", true),
            ("pkg/sum_test.go", true),
            ("pkg/sum.go", false),
            ("tests/test_x.py", true),
            ("x_test.py", true),
            ("app.py", false),
            ("src/a.test.ts", true),
            ("src/__tests__/a.js", true),
            ("src/a.js", false),
            ("README.md", false),
        ] {
            assert_eq!(may_hold_tests(Path::new(path)), yes, "{path}");
        }
    }
}
