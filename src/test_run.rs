//! Test results from what a Run tab printed: cargo test (libtest), go test
//! -v, pytest -v, and Jest, Vitest's verbose reporter and `node --test`
//! (their ✓ / ✕ lines). `Framework::of` tells the runner by its command,
//! `parse` reads each test's status, what a failed one printed and where it
//! failed; `tree` lays the results out for the Find panel, `key` and
//! `status_of` match them to the gutter's run targets (`RunTarget::test`),
//! and `rerun_failed` makes the command that runs only the failed ones.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::{
    assets::Icon,
    lsp::protocol::{Encoding, Position},
    results_panel::{Leading, ResultNode, ResultTarget, Tone},
    run::{RunTarget, shell_quote},
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Framework {
    Cargo,
    Go,
    Pytest,
    Js,
}

impl Framework {
    /// The test runner a command line starts, if it's one Kuana reads.
    pub fn of(command: &str) -> Option<Framework> {
        let words: Vec<&str> = command.split_whitespace().collect();
        match words.as_slice() {
            // `oxie test` runs libtest over the Rust that Oxie compiles to
            ["cargo", "test", ..] | ["oxie", "test", ..] => Some(Framework::Cargo),
            // `lyth test` runs go test over the Go that Lyth compiles to
            ["go", "test", ..] | ["lyth", "test", ..] => Some(Framework::Go),
            ["pytest", ..] | [_, "-m", "pytest", ..] => Some(Framework::Pytest),
            ["node", ..] if words.contains(&"--test") => Some(Framework::Js),
            _ if words.iter().any(|w| matches!(*w, "jest" | "vitest")) => Some(Framework::Js),
            _ => None,
        }
    }

    /// What follows a test's id in the ids of the tests under it (a module's
    /// tests, subtests, a class's methods, parameters).
    fn separators(self) -> &'static [&'static str] {
        match self {
            Framework::Cargo => &["::"],
            Framework::Go => &["/"],
            Framework::Pytest => &["::", "["],
            Framework::Js => &[],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

impl TestStatus {
    pub fn icon(self) -> (Icon, Tone) {
        match self {
            TestStatus::Passed => (Icon::CircleCheck, Tone::Success),
            TestStatus::Failed => (Icon::CircleX, Tone::Error),
            TestStatus::Skipped => (Icon::Minus, Tone::Muted),
        }
    }

    /// The status of tests together: failed if one did, else passed if one
    /// did, else skipped.
    pub fn combine(a: Option<TestStatus>, b: TestStatus) -> TestStatus {
        match (a, b) {
            (Some(TestStatus::Failed), _) | (_, TestStatus::Failed) => TestStatus::Failed,
            (Some(TestStatus::Passed), _) | (_, TestStatus::Passed) => TestStatus::Passed,
            _ => TestStatus::Skipped,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    /// As the runner names it: `tests::adds`, `TestAdd/sub`,
    /// `tests/test_x.py::TestA::test_b`, a title.
    pub id: String,
    pub status: TestStatus,
    /// What a failed test printed (its panic, its assertion).
    pub output: Vec<String>,
    /// Where it failed: a file and a 1-based line.
    pub location: Option<(PathBuf, u32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestRun {
    pub framework: Framework,
    pub results: Vec<TestResult>,
    /// pytest's rootdir, which its node ids are relative to.
    pub root: Option<PathBuf>,
}

impl TestRun {
    pub fn count(&self, status: TestStatus) -> usize {
        self.results.iter().filter(|r| r.status == status).count()
    }

    /// "2 failed, 10 passed, 1 skipped".
    pub fn summary(&self) -> String {
        let parts: Vec<String> = [(TestStatus::Failed, "failed"), (TestStatus::Passed, "passed"), (TestStatus::Skipped, "skipped")]
            .into_iter()
            .map(|(status, word)| (self.count(status), word))
            .filter(|(n, _)| *n > 0)
            .map(|(n, word)| format!("{n} {word}"))
            .collect();
        parts.join(", ")
    }
}

/// The lines of a failure kept, at most.
const MAX_OUTPUT: usize = 40;

/// Reads a run's output (as the terminal shows it). `cwd` is where it ran,
/// for the files in failures.
pub fn parse(framework: Framework, output: &str, cwd: &Path) -> TestRun {
    let mut root = None;
    let mut results = match framework {
        Framework::Cargo => cargo(output, cwd),
        Framework::Go => go(output, cwd),
        Framework::Pytest => {
            root = output.lines().find_map(|l| l.strip_prefix("rootdir: ")).map(|r| PathBuf::from(r.split(", ").next().unwrap_or(r).trim()));
            pytest(output, root.as_deref().unwrap_or(cwd))
        }
        Framework::Js => js(output, cwd),
    };
    for result in &mut results {
        if result.status != TestStatus::Failed {
            result.output.clear();
        }
        while result.output.first().is_some_and(|l| l.trim().is_empty()) {
            result.output.remove(0);
        }
        while result.output.last().is_some_and(|l| l.trim().is_empty()) {
            result.output.pop();
        }
        result.output.truncate(MAX_OUTPUT);
    }
    TestRun { framework, results, root }
}

/// A result by id, added at the end if new (a test reported twice keeps its place).
fn entry<'a>(results: &'a mut Vec<TestResult>, id: &str, status: TestStatus) -> &'a mut TestResult {
    match results.iter().position(|r| r.id == id) {
        Some(ix) => {
            results[ix].status = status;
            &mut results[ix]
        }
        None => {
            results.push(TestResult { id: id.to_string(), status, output: Vec::new(), location: None });
            results.last_mut().expect("just pushed")
        }
    }
}

/// A file a runner names (absolute, or relative to where it ran or a folder
/// above, as cargo's workspace members print paths from the workspace root).
fn find_file(name: &str, cwd: &Path) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    cwd.ancestors().map(|dir| dir.join(path)).find(|p| p.is_file())
}

/// `path:line` or `path:line:column` at the start of `text`, for a file that exists.
fn file_line(text: &str, cwd: &Path) -> Option<(PathBuf, u32)> {
    let mut parts = text.splitn(3, ':');
    let (file, line) = (parts.next()?.trim(), parts.next()?);
    let line: u32 = line.trim().parse().ok()?;
    Some((find_file(file, cwd)?, line))
}

// ---- cargo test ---------------------------------------------------------------------

/// libtest: `test tests::adds ... ok` lines, then each failure's output under
/// `---- tests::fails stdout ----`, with its panic's place.
fn cargo(output: &str, cwd: &Path) -> Vec<TestResult> {
    let mut results: Vec<TestResult> = Vec::new();
    let mut block: Option<usize> = None;
    for line in output.lines() {
        if let Some((name, status)) = line.strip_prefix("test ").and_then(|l| l.rsplit_once(" ... ")) {
            let status = match status.trim() {
                "ok" => TestStatus::Passed,
                s if s.starts_with("FAILED") => TestStatus::Failed,
                s if s.starts_with("ignored") => TestStatus::Skipped,
                _ => continue,
            };
            entry(&mut results, name.trim_end_matches(" - should panic"), status);
            block = None;
            continue;
        }
        if let Some(name) = line.strip_prefix("---- ").and_then(|l| l.strip_suffix(" stdout ----").or_else(|| l.strip_suffix(" stderr ----"))) {
            block = results.iter().position(|r| r.id == name);
            continue;
        }
        if line == "failures:" || line.starts_with("test result:") || line.starts_with("running ") {
            block = None;
            continue;
        }
        let Some(ix) = block else { continue };
        if line.starts_with("note: run with `RUST_BACKTRACE") {
            continue;
        }
        let result = &mut results[ix];
        if result.location.is_none()
            && let Some(at) = line.split_once(" panicked at ").map(|(_, at)| at)
        {
            // `src/lib.rs:12:9:` (Rust 1.73+), or `'message', src/lib.rs:12:9` before.
            let place = at.rsplit_once("', ").map_or(at, |(_, place)| place);
            result.location = file_line(place.trim_end_matches(':'), cwd);
        }
        result.output.push(line.to_string());
    }
    results
}

// ---- go test -v -----------------------------------------------------------------------

/// `=== RUN TestX`, then `--- PASS: TestX (0.00s)` (indented for subtests);
/// a test's logs come between the two (or indented after its `--- FAIL`).
fn go(output: &str, cwd: &Path) -> Vec<TestResult> {
    let mut results: Vec<TestResult> = Vec::new();
    let mut logs: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(name) = ["=== RUN", "=== CONT", "=== PAUSE"].iter().find_map(|p| trimmed.strip_prefix(p)) {
            current = Some(name.trim().to_string());
            continue;
        }
        let status = [("--- PASS: ", TestStatus::Passed), ("--- FAIL: ", TestStatus::Failed), ("--- SKIP: ", TestStatus::Skipped)]
            .into_iter()
            .find_map(|(prefix, status)| Some((trimmed.strip_prefix(prefix)?, status)));
        if let Some((rest, status)) = status {
            let name = rest.split(" (").next().unwrap_or(rest).trim().to_string();
            entry(&mut results, &name, status);
            current = Some(name);
            continue;
        }
        if !line.starts_with("    ") {
            // `FAIL`, `ok  pkg`, `PASS`: between tests.
            current = None;
            continue;
        }
        if let Some(name) = &current {
            logs.entry(name.clone()).or_default().push(trimmed.to_string());
        }
    }
    for result in &mut results {
        result.output = logs.remove(&result.id).unwrap_or_default();
        result.location = result.output.iter().find_map(|l| file_line(l, cwd));
    }
    results
}

// ---- pytest -v ------------------------------------------------------------------------

/// `tests/test_x.py::TestA::test_b PASSED [ 50%]` lines, then each failure
/// under `____ TestA.test_b ____` in the FAILURES section, ending with
/// `tests/test_x.py:7: AssertionError`.
fn pytest(output: &str, root: &Path) -> Vec<TestResult> {
    let mut results: Vec<TestResult> = Vec::new();
    let statuses = [
        ("PASSED", TestStatus::Passed),
        ("FAILED", TestStatus::Failed),
        ("ERROR", TestStatus::Failed),
        ("SKIPPED", TestStatus::Skipped),
        ("XFAIL", TestStatus::Skipped),
        ("XPASS", TestStatus::Passed),
    ];
    let mut in_failures = false;
    let mut block: Option<usize> = None;
    for line in output.lines() {
        if line.starts_with('=') {
            in_failures = line.contains(" FAILURES ") || line.contains(" ERRORS ");
            block = None;
            continue;
        }
        if !in_failures {
            let mut words = line.split_whitespace();
            if let (Some(id), Some(word)) = (words.next(), words.next())
                && id.contains("::")
                && let Some((_, status)) = statuses.iter().find(|(w, _)| *w == word)
            {
                entry(&mut results, id, *status);
            }
            continue;
        }
        // `____ TestA.test_b ____`: the test whose node path, dotted, it names.
        if line.starts_with("___") && line.ends_with("___") {
            let name = line.trim_matches(|c: char| c == '_' || c.is_whitespace());
            let name = name.strip_prefix("ERROR at setup of ").unwrap_or(name);
            block = results.iter().position(|r| r.id.split_once("::").is_some_and(|(_, path)| path.replace("::", ".") == name));
            continue;
        }
        let Some(ix) = block else { continue };
        let result = &mut results[ix];
        if let Some((file, rest)) = line.split_once(".py:")
            && !file.contains(char::is_whitespace)
            && let Some((number, _)) = rest.split_once(": ")
            && let Some(place) = file_line(&format!("{file}.py:{number}"), root)
        {
            result.location = Some(place);
        }
        result.output.push(line.to_string());
    }
    results
}

// ---- Jest, Vitest, node --test -------------------------------------------------------

/// Its ✓ / ✕ lines (Jest, Vitest's verbose reporter, node's spec reporter),
/// and the failures they print after: Jest's `● Suite › title`, Vitest's
/// ` FAIL  file > suite > title`, node's `✖ title` under `failing tests:`.
fn js(output: &str, cwd: &Path) -> Vec<TestResult> {
    let mut results: Vec<TestResult> = Vec::new();
    let mut details = false;
    let mut block: Option<usize> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("failing tests:") {
            details = true;
            continue;
        }
        let header = trimmed
            .strip_prefix("● ")
            .map(|t| t.rsplit(" › ").next().unwrap_or(t))
            .or_else(|| trimmed.strip_prefix("FAIL ").map(|t| t.rsplit(" > ").next().unwrap_or(t)));
        if let Some(title) = header {
            block = results.iter().position(|r| r.id == title.trim());
            continue;
        }
        let mark = trimmed.chars().next().unwrap_or(' ');
        let status = match mark {
            '✓' | '✔' => Some(TestStatus::Passed),
            '✕' | '✖' | '×' => Some(TestStatus::Failed),
            '○' | '↓' | '﹣' => Some(TestStatus::Skipped),
            _ => None,
        };
        if let Some(status) = status {
            let title = js_title(trimmed[mark.len_utf8()..].trim());
            if details {
                // node repeats failed tests with what went wrong under them.
                block = results.iter().position(|r| r.id == title);
            } else if !title.is_empty() {
                entry(&mut results, &title, status);
                block = None;
            }
            continue;
        }
        if trimmed.starts_with("Test Suites:") || trimmed.starts_with("Tests:") || trimmed.starts_with("ℹ tests") {
            block = None;
            continue;
        }
        let Some(ix) = block else { continue };
        let result = &mut results[ix];
        if result.location.is_none() {
            // `at Object.<anonymous> (src/a.test.js:5:13)`, `at file:///…/a.test.js:5:13`, `❯ src/a.test.ts:5:13`.
            let place = trimmed.rsplit_once('(').map_or(trimmed, |(_, p)| p.trim_end_matches(')'));
            let place = place.trim_start_matches("at ").trim_start_matches("❯ ").trim_start_matches("file://");
            if !place.contains("node_modules") && !place.starts_with("node:") {
                result.location = file_line(place, cwd);
            }
        }
        result.output.push(line.to_string());
    }
    results
}

/// A title without its time (`(3 ms)`, ` 5ms`) and markers (`# SKIP`, `skipped `).
fn js_title(text: &str) -> String {
    let mut title = text.trim_start_matches("skipped ").trim();
    if let Some(cut) = title.find(" # ") {
        title = &title[..cut];
    }
    if title.ends_with(')')
        && let Some((rest, time)) = title.rsplit_once(" (")
        && time.trim_end_matches(')').trim_end_matches("ms").trim_end_matches('s').trim().parse::<f64>().is_ok()
    {
        title = rest;
    }
    if let Some((rest, time)) = title.rsplit_once(' ')
        && time.strip_suffix("ms").is_some_and(|n| n.parse::<f64>().is_ok())
    {
        title = rest;
    }
    // Vitest's verbose reporter names the file and suites: `a.test.ts > math > adds`.
    title.rsplit(" > ").next().unwrap_or(title).trim().to_string()
}

// ---- Matching run targets -------------------------------------------------------------

/// The last status of each test, by `(framework, key)`.
pub type Statuses = HashMap<(Framework, String), TestStatus>;

/// How a result is named for matching run targets: pytest's node id by its
/// file's name and node path (ids are relative to the rootdir), else its id.
pub fn key(framework: Framework, id: &str) -> String {
    match framework {
        Framework::Pytest => match id.split_once("::") {
            Some((file, rest)) => format!("{}::{rest}", file.rsplit(['/', '\\']).next().unwrap_or(file)),
            None => id.to_string(),
        },
        _ => id.to_string(),
    }
}

/// Puts a run's results into the statuses kept (newer ones win).
pub fn record(statuses: &mut Statuses, run: &TestRun) {
    for result in &run.results {
        statuses.insert((run.framework, key(run.framework, &result.id)), result.status);
    }
}

/// A run target's mark: its test's status, or that of the tests under it
/// (a module, a class, subtests).
pub fn status_of(test: &(Framework, String), statuses: &Statuses) -> Option<TestStatus> {
    let (framework, key) = test;
    statuses
        .iter()
        .filter(|((f, k), _)| {
            f == framework && (k == key || framework.separators().iter().any(|sep| k.len() > key.len() && k.starts_with(key.as_str()) && k[key.len()..].starts_with(sep)))
        })
        .fold(None, |acc, (_, status)| Some(TestStatus::combine(acc, *status)))
}

// ---- Rerun Failed ---------------------------------------------------------------------

/// The command that runs only `run`'s failed tests, the way `target` ran
/// them. None when none failed or the runner's filter can't say it.
pub fn rerun_failed(target: &RunTarget, run: &TestRun) -> Option<RunTarget> {
    let mut failed: Vec<&str> = run.results.iter().filter(|r| r.status == TestStatus::Failed).map(|r| r.id.as_str()).collect();
    if failed.is_empty() {
        return None;
    }
    let command = match run.framework {
        Framework::Cargo => {
            let (base, harness) = target.command.split_once(" -- ").unwrap_or((&target.command, ""));
            let names: Vec<String> = failed.iter().map(|id| shell_quote(id)).collect();
            format!("{} -- {} --exact{}", cargo_base(base), names.join(" "), harness_options(harness))
        }
        Framework::Go => {
            // Subtests run with their test.
            let mut tops: Vec<&str> = failed.iter().map(|id| id.split('/').next().unwrap_or(id)).collect();
            tops.dedup();
            let pattern = format!("^({})$", tops.join("|"));
            let runner = if target.command.starts_with("lyth test") { "lyth test" } else { "go test" };
            format!("{runner} -v -run {} {}", shell_quote(&pattern), go_packages(&target.command))
        }
        Framework::Pytest => {
            // Node ids are relative to the rootdir; the run may start elsewhere.
            let root = run.root.as_deref().unwrap_or(&target.cwd);
            let python = target.command.split_whitespace().next().filter(|w| *w != "pytest").map(|w| format!("{w} -m pytest")).unwrap_or("pytest".into());
            let ids: Vec<String> = failed.iter().map(|id| shell_quote(&root.join(id).to_string_lossy())).collect();
            format!("{python} -v {}", ids.join(" "))
        }
        Framework::Js => {
            failed.dedup();
            let pattern = failed.iter().map(|t| regex::escape(t)).collect::<Vec<_>>().join("|");
            if let Some((base, _)) = target.command.split_once(" -t ") {
                format!("{base} -t {}", shell_quote(&pattern))
            } else if target.command.contains("--test-name-pattern=") {
                let words: Vec<&str> = target.command.split_whitespace().filter(|w| !w.starts_with("--test-name-pattern=")).collect();
                let (first, rest) = words.split_first()?;
                format!("{first} --test-name-pattern={} {}", shell_quote(&pattern), rest.join(" ")).trim_end().to_string()
            } else if target.command.split_whitespace().any(|w| matches!(w, "jest" | "vitest")) {
                format!("{} -t {}", target.command, shell_quote(&pattern))
            } else {
                return None;
            }
        }
    };
    Some(RunTarget {
        name: format!("Failed tests of {}", target.name.trim_start_matches("Failed tests of ")),
        command,
        cwd: target.cwd.clone(),
        row: None,
        rows: 0..0,
        debug: None,
        test: None,
    })
}

/// The part of a `cargo test` / `oxie test` command line before the
/// harness's, without the test it named: `oxie test <file> <name>` takes the
/// name as a filter, which the failed ones' ids replace.
fn cargo_base(base: &str) -> &str {
    if !base.starts_with("oxie test") {
        return base;
    }
    match base.find(".oxi") {
        // Up to the file's end, its closing quote included.
        Some(at) => &base[..at + 4 + usize::from(base[at + 4..].starts_with('\''))],
        None => "oxie test",
    }
}

/// The harness options a run passed (`--format=pretty`, `--nocapture`),
/// with their values, for its rerun; its filters and `--exact` go.
fn harness_options(harness: &str) -> String {
    let mut kept = String::new();
    let mut words = harness.split_whitespace();
    while let Some(word) = words.next() {
        if !word.starts_with('-') || word == "--exact" {
            continue;
        }
        kept.push(' ');
        kept.push_str(word);
        if matches!(word, "--format" | "--color" | "--test-threads" | "--skip" | "--logfile" | "-Z")
            && let Some(value) = words.next()
        {
            kept.push(' ');
            kept.push_str(value);
        }
    }
    kept
}

/// The packages a `go test` command line names (`.` when none).
fn go_packages(command: &str) -> String {
    let mut words = command.split_whitespace().skip(2);
    let mut packages = Vec::new();
    while let Some(word) = words.next() {
        if matches!(word, "-run" | "-bench" | "-count" | "-timeout" | "-tags") {
            words.next();
        } else if !word.starts_with('-') {
            packages.push(word);
        }
    }
    if packages.is_empty() { ".".into() } else { packages.join(" ") }
}

// ---- The Find panel's tree ------------------------------------------------------------

/// The results as a tree: groups by the id's parts (a module path, a test
/// and its subtests, a file and class), each test with its mark; a failed
/// test opens onto what it printed and goes to where it failed.
pub fn tree(run: &TestRun) -> Vec<ResultNode> {
    let mut roots: Vec<ResultNode> = Vec::new();
    for result in &run.results {
        let parts: Vec<&str> = match run.framework {
            Framework::Cargo | Framework::Pytest => result.id.split("::").collect(),
            Framework::Go => result.id.split('/').collect(),
            Framework::Js => vec![result.id.as_str()],
        };
        let (leaf, groups) = parts.split_last().expect("split gives a part");
        let mut level = &mut roots;
        for group in groups {
            let ix = match level.iter().position(|n| is_group(n) && n.label.as_ref() == *group) {
                Some(ix) => ix,
                None => {
                    level.push(group_node(group));
                    level.len() - 1
                }
            };
            level = &mut level[ix].children;
        }
        level.push(test_node(leaf, result));
    }
    for root in &mut roots {
        mark_groups(root);
    }
    roots
}

/// Groups have a folder until `mark_groups` gives them their tests' mark;
/// tests have theirs from the start.
fn is_group(node: &ResultNode) -> bool {
    node.leading == Leading::Icon(Icon::Folder)
}

fn group_node(label: &str) -> ResultNode {
    ResultNode {
        leading: Leading::Icon(Icon::Folder),
        label: label.to_string().into(),
        highlight: None,
        detail: None,
        target: None,
        open: None,
        children: Vec::new(),
        loader: None,
        expanded: true,
    }
}

fn test_node(label: &str, result: &TestResult) -> ResultNode {
    let (icon, tone) = result.status.icon();
    let target = result.location.as_ref().map(|(path, line)| ResultTarget {
        path: path.clone(),
        position: Position { line: line.saturating_sub(1), character: 0 },
        encoding: Encoding::Utf8,
    });
    let detail = result.location.as_ref().map(|(path, line)| {
        format!("{}:{line}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()).into()
    });
    ResultNode {
        leading: Leading::Tinted(icon, tone),
        label: label.to_string().into(),
        highlight: None,
        detail,
        target,
        open: None,
        children: result.output.iter().map(|line| ResultNode::message(line.clone())).collect(),
        loader: None,
        expanded: result.status == TestStatus::Failed,
    }
}

/// Gives each group the mark of its tests; returns it.
fn mark_groups(node: &mut ResultNode) -> Option<TestStatus> {
    match node.leading {
        Leading::Tinted(_, Tone::Error) => return Some(TestStatus::Failed),
        Leading::Tinted(_, Tone::Success) => return Some(TestStatus::Passed),
        Leading::Tinted(..) => return Some(TestStatus::Skipped),
        _ => {}
    }
    let mut status = None;
    for child in &mut node.children {
        if let Some(s) = mark_groups(child) {
            status = Some(TestStatus::combine(status, s));
        }
    }
    if let Some(status) = status {
        let (icon, tone) = status.icon();
        node.leading = Leading::Tinted(icon, tone);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-test-run-{}-{}", std::process::id(), files.join("-").replace('/', "_")));
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        dir
    }

    #[test]
    fn knows_runners_by_command() {
        assert_eq!(Framework::of("cargo test -- tests::a --exact"), Some(Framework::Cargo));
        assert_eq!(Framework::of("go test -v ./..."), Some(Framework::Go));
        assert_eq!(Framework::of("python3 -m pytest -v /p/t.py::test_a"), Some(Framework::Pytest));
        assert_eq!(Framework::of("npx vitest run --reporter=verbose a.test.ts -t x"), Some(Framework::Js));
        assert_eq!(Framework::of("node --test --test-name-pattern=x a.test.js"), Some(Framework::Js));
        assert_eq!(Framework::of("cargo run"), None);
        assert_eq!(Framework::of("node app.js"), None);
    }

    #[test]
    fn reads_cargo_test() {
        let dir = temp(&["src/lib.rs"]);
        let output = "\
running 3 tests
test tests::adds ... ok
test tests::fails ... FAILED
test tests::slow ... ignored, takes long

failures:

---- tests::fails stdout ----

thread 'tests::fails' panicked at src/lib.rs:12:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    tests::fails

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
";
        let run = parse(Framework::Cargo, output, &dir);
        let statuses: Vec<_> = run.results.iter().map(|r| (r.id.as_str(), r.status)).collect();
        assert_eq!(statuses, [("tests::adds", TestStatus::Passed), ("tests::fails", TestStatus::Failed), ("tests::slow", TestStatus::Skipped)]);
        let failed = &run.results[1];
        assert_eq!(failed.location, Some((dir.join("src/lib.rs"), 12)));
        assert_eq!(failed.output.first().map(String::as_str), Some("thread 'tests::fails' panicked at src/lib.rs:12:9:"));
        assert_eq!(failed.output.last().map(String::as_str), Some(" right: 2"));
        assert_eq!(run.summary(), "1 failed, 1 passed, 1 skipped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_go_test() {
        let dir = temp(&["math_test.go"]);
        let output = "\
=== RUN   TestAdd
--- PASS: TestAdd (0.00s)
=== RUN   TestTable
=== RUN   TestTable/zero
    math_test.go:20: got 1, want 0
=== RUN   TestTable/one
--- FAIL: TestTable (0.00s)
    --- FAIL: TestTable/zero (0.00s)
    --- PASS: TestTable/one (0.00s)
=== RUN   TestSkip
    math_test.go:30: not today
--- SKIP: TestSkip (0.00s)
FAIL
exit status 1
FAIL\texample.com/m\t0.002s
";
        let run = parse(Framework::Go, output, &dir);
        let statuses: Vec<_> = run.results.iter().map(|r| (r.id.as_str(), r.status)).collect();
        assert_eq!(
            statuses,
            [
                ("TestAdd", TestStatus::Passed),
                ("TestTable", TestStatus::Failed),
                ("TestTable/zero", TestStatus::Failed),
                ("TestTable/one", TestStatus::Passed),
                ("TestSkip", TestStatus::Skipped),
            ]
        );
        assert_eq!(run.results[2].output, ["math_test.go:20: got 1, want 0"]);
        assert_eq!(run.results[2].location, Some((dir.join("math_test.go"), 20)));
        assert!(run.results[4].output.is_empty(), "only failures keep their output");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_pytest() {
        let dir = temp(&["tests/test_x.py"]);
        let output = format!(
            "\
============================= test session starts ==============================
rootdir: {}, configfile: pyproject.toml
collected 3 items

tests/test_x.py::test_ok PASSED                                          [ 33%]
tests/test_x.py::TestA::test_b FAILED                                    [ 66%]
tests/test_x.py::test_p[1] SKIPPED (no)                                  [100%]

=================================== FAILURES ===================================
_________________________________ TestA.test_b _________________________________

self = <test_x.TestA object at 0x1>

    def test_b(self):
>       assert 1 == 2
E       assert 1 == 2

tests/test_x.py:7: AssertionError
=========================== short test summary info ============================
FAILED tests/test_x.py::TestA::test_b - assert 1 == 2
",
            dir.display()
        );
        let run = parse(Framework::Pytest, &output, Path::new("/elsewhere"));
        assert_eq!(run.root.as_deref(), Some(dir.as_path()));
        let statuses: Vec<_> = run.results.iter().map(|r| (r.id.as_str(), r.status)).collect();
        assert_eq!(
            statuses,
            [("tests/test_x.py::test_ok", TestStatus::Passed), ("tests/test_x.py::TestA::test_b", TestStatus::Failed), ("tests/test_x.py::test_p[1]", TestStatus::Skipped)]
        );
        let failed = &run.results[1];
        assert_eq!(failed.location, Some((dir.join("tests/test_x.py"), 7)));
        assert!(failed.output.iter().any(|l| l == "E       assert 1 == 2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_jest_vitest_and_node() {
        let dir = temp(&["src/a.test.js"]);
        let jest = "\
 FAIL  src/a.test.js
  math
    ✓ adds (2 ms)
    ✕ subtracts (5 ms)
    ○ skipped divides

  ● math › subtracts

    expect(received).toBe(expected)

      at Object.<anonymous> (src/a.test.js:5:13)

Tests:       1 failed, 1 skipped, 1 passed, 3 total
";
        let run = parse(Framework::Js, jest, &dir);
        let statuses: Vec<_> = run.results.iter().map(|r| (r.id.as_str(), r.status)).collect();
        assert_eq!(statuses, [("adds", TestStatus::Passed), ("subtracts", TestStatus::Failed), ("divides", TestStatus::Skipped)]);
        assert_eq!(run.results[1].location, Some((dir.join("src/a.test.js"), 5)));

        let vitest = " ✓ src/a.test.ts > math > adds 1ms\n × src/a.test.ts > math > subtracts 3ms\n";
        let statuses: Vec<_> = parse(Framework::Js, vitest, &dir).results.into_iter().map(|r| (r.id, r.status)).collect();
        assert_eq!(statuses, [("adds".to_string(), TestStatus::Passed), ("subtracts".to_string(), TestStatus::Failed)]);

        let node = "▶ math\n  ✔ adds (0.5ms)\n  ✖ subtracts (1.2ms)\n▶ math (2ms)\nℹ tests 2\n✖ failing tests:\n\ntest at src/a.test.js:5:3\n✖ subtracts (1.2ms)\n  AssertionError: 1 == 2\n";
        let run = parse(Framework::Js, node, &dir);
        assert_eq!(run.results.len(), 2);
        assert_eq!(run.results[1].output, ["  AssertionError: 1 == 2"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn marks_targets_and_groups() {
        let run = TestRun {
            framework: Framework::Cargo,
            results: vec![
                TestResult { id: "tests::adds".into(), status: TestStatus::Passed, output: vec![], location: None },
                TestResult { id: "tests::fails".into(), status: TestStatus::Failed, output: vec![], location: None },
                TestResult { id: "testsuite::x".into(), status: TestStatus::Passed, output: vec![], location: None },
            ],
            root: None,
        };
        let mut statuses = Statuses::new();
        record(&mut statuses, &run);
        let of = |key: &str| status_of(&(Framework::Cargo, key.to_string()), &statuses);
        assert_eq!(of("tests::adds"), Some(TestStatus::Passed));
        assert_eq!(of("tests"), Some(TestStatus::Failed), "a module fails with one of its tests");
        assert_eq!(of("testsuite"), Some(TestStatus::Passed), "a name that only starts the same isn't under it");
        assert_eq!(of("other"), None);
        // pytest goes by file name; parameters count for their test.
        let mut py = Statuses::new();
        record(&mut py, &TestRun { framework: Framework::Pytest, results: vec![TestResult { id: "tests/t.py::test_p[1]".into(), status: TestStatus::Failed, output: vec![], location: None }], root: None });
        assert_eq!(status_of(&(Framework::Pytest, "t.py::test_p".into()), &py), Some(TestStatus::Failed));

        let tree = tree(&run);
        assert_eq!(tree.iter().map(|n| n.label.to_string()).collect::<Vec<_>>(), ["tests", "testsuite"]);
        assert_eq!(tree[0].leading, Leading::Tinted(Icon::CircleX, Tone::Error));
        assert_eq!(tree[0].children.len(), 2);
    }

    #[test]
    fn reruns_only_the_failed() {
        let failed = |framework, ids: &[&str], root: Option<&str>| TestRun {
            framework,
            results: ids.iter().map(|id| TestResult { id: id.to_string(), status: TestStatus::Failed, output: vec![], location: None }).collect(),
            root: root.map(PathBuf::from),
        };
        let target = |command: &str| RunTarget { name: "Tests".into(), command: command.into(), cwd: "/p".into(), row: None, rows: 0..0, debug: None, test: None };
        let cargo = rerun_failed(&target("cargo test --lib -- tests"), &failed(Framework::Cargo, &["tests::a", "tests::b"], None)).unwrap();
        assert_eq!(cargo.command, "cargo test --lib -- tests::a tests::b --exact");
        let go = rerun_failed(&target("go test -v -run '^TestX$' ./pkg"), &failed(Framework::Go, &["TestX", "TestX/sub", "TestY"], None)).unwrap();
        assert_eq!(go.command, "go test -v -run '^(TestX|TestY)$' ./pkg");
        let py = rerun_failed(&target("python3 -m pytest -v"), &failed(Framework::Pytest, &["tests/t.py::test_a"], Some("/root"))).unwrap();
        // Joined like the code does: Windows puts a `\` after the root.
        let id = shell_quote(&Path::new("/root").join("tests/t.py::test_a").to_string_lossy());
        assert_eq!(py.command, format!("python3 -m pytest -v {id}"));
        let jest = rerun_failed(&target("npx jest /p/a.test.js -t old"), &failed(Framework::Js, &["adds 1+1"], None)).unwrap();
        assert_eq!(jest.command, "npx jest /p/a.test.js -t 'adds 1\\+1'");
        let node = rerun_failed(&target("node --test --test-name-pattern=old /p/a.test.js"), &failed(Framework::Js, &["adds"], None)).unwrap();
        assert_eq!(node.command, "node --test-name-pattern=adds --test /p/a.test.js");
        assert!(rerun_failed(&target("cargo test"), &failed(Framework::Cargo, &[], None)).is_none());
        // The run's options stay; `oxie test`'s name filter goes.
        let options = rerun_failed(&target("cargo test -- tests --test-threads 1 --nocapture"), &failed(Framework::Cargo, &["tests::a"], None)).unwrap();
        assert_eq!(options.command, "cargo test -- tests::a --exact --test-threads 1 --nocapture");
        let ids = ["__oxie_tests::test_adds_two"];
        let oxie = rerun_failed(&target("oxie test '/my p/t.oxi' 'adds two' -- --format=pretty"), &failed(Framework::Cargo, &ids, None)).unwrap();
        assert_eq!(oxie.command, "oxie test '/my p/t.oxi' -- __oxie_tests::test_adds_two --exact --format=pretty");
        let project = rerun_failed(&target("oxie test -- --format=pretty"), &failed(Framework::Cargo, &["a::__oxie_tests::test_b"], None)).unwrap();
        assert_eq!(project.command, "oxie test -- a::__oxie_tests::test_b --exact --format=pretty");
        // `lyth test` is go test over the generated Go, and reruns with lyth, which compiles the .lyth files.
        assert_eq!(Framework::of("lyth test -v ./..."), Some(Framework::Go));
        let lyth = rerun_failed(&target("lyth test -v -run '^TestAdd$' ."), &failed(Framework::Go, &["TestAdd", "TestSub/negative"], None)).unwrap();
        assert_eq!(lyth.command, "lyth test -v -run '^(TestAdd|TestSub)$' .");
    }
}
