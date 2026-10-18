//! Git support built on the `git` command line, so the user's config,
//! credential helpers, SSH keys and hooks all apply. Every function here
//! blocks; call them from a background task.

use std::{
    collections::HashMap,
    io::{Read, Write},
    ops::Range,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::encoding::TextEncoding;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl Change {
    pub fn letter(self) -> &'static str {
        match self {
            Change::Added => "A",
            Change::Modified => "M",
            Change::Deleted => "D",
            Change::Renamed => "R",
            Change::Untracked => "U",
            Change::Conflicted => "C",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Change::Added => "Added",
            Change::Modified => "Modified",
            Change::Deleted => "Deleted",
            Change::Renamed => "Renamed",
            Change::Untracked => "Untracked",
            Change::Conflicted => "Conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the repository root, with forward slashes.
    pub rel: String,
    /// Change recorded in the index (staged), if any.
    pub staged: Option<Change>,
    /// Change in the working tree not yet staged, if any.
    pub unstaged: Option<Change>,
    /// The file's blob ids at HEAD and in the index ("hH hI"; for a conflict
    /// its stages'): staging part of it elsewhere (`git add -p`) changes
    /// these even when the kind of change stays the same. None if untracked.
    pub blobs: Option<String>,
}

impl FileStatus {
    /// The most relevant change for display.
    pub fn primary(&self) -> Change {
        match (self.staged, self.unstaged) {
            (Some(Change::Conflicted), _) | (_, Some(Change::Conflicted)) => Change::Conflicted,
            (_, Some(c)) => c,
            (Some(c), None) => c,
            (None, None) => Change::Modified,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
    /// What Git ignores (the global excludes and nested .gitignore files
    /// included): files, and folders as a whole, nothing listed inside them.
    pub ignored: Vec<PathBuf>,
    /// A merge or rebase that stopped and waits to be continued or aborted.
    pub state: Option<RepoState>,
}

impl RepoStatus {
    pub fn staged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.staged.is_some() && f.staged != Some(Change::Conflicted))
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.staged == Some(Change::Conflicted))
    }

    /// Tracked files with changes not staged yet, and conflicts.
    pub fn unstaged(&self) -> impl Iterator<Item = &FileStatus> {
        self.files
            .iter()
            .filter(|f| (f.unstaged.is_some() && f.unstaged != Some(Change::Untracked)) || f.staged == Some(Change::Conflicted))
    }

    /// Files Git doesn't track (unversioned). A commit takes them only when
    /// asked to, unlike changes to tracked files.
    pub fn untracked(&self) -> impl Iterator<Item = &FileStatus> {
        self.files.iter().filter(|f| f.unstaged == Some(Change::Untracked))
    }

    /// Status per path, plus a "contains changes" mark for every ancestor
    /// directory (used to tint folders in the file tree).
    pub fn path_map(&self, root: &Path) -> HashMap<PathBuf, Change> {
        let mut map = HashMap::new();
        for f in &self.files {
            let change = f.primary();
            map.insert(f.path.clone(), change);
            for dir in f.path.ancestors().skip(1) {
                if dir == root || !dir.starts_with(root) {
                    break;
                }
                let entry = map.entry(dir.to_path_buf()).or_insert(change);
                if change == Change::Conflicted {
                    *entry = Change::Conflicted;
                } else if *entry != Change::Conflicted {
                    *entry = Change::Modified;
                }
            }
        }
        map
    }
}

#[derive(Clone, Debug)]
pub struct Branch {
    pub name: String,
    pub remote: bool,
    pub current: bool,
}

/// Environment variables that choose the repository instead of the folder.
const REPOSITORY_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// What a repository's own config may name to run, turned off where the
/// folder isn't trusted (Restricted Mode): hooks, an SSH command,
/// credential helpers, the `ext::` transport and external diff programs.
const UNTRUSTED_CONFIG: &[&str] =
    &["core.hooksPath=/dev/null", "core.sshCommand=ssh", "credential.helper=", "protocol.ext.allow=never", "diff.external="];

/// `git <args>` in `root`.
fn command(root: &Path, args: &[&str]) -> Command {
    command_in(root, args, crate::trust::is_trusted(root))
}

/// `git <args>` in `root`, a folder trusted or not.
fn command_in(root: &Path, args: &[&str], trusted: bool) -> Command {
    let mut cmd = crate::process::command("git");
    // A repository's config can name a program for `git status` to run; and
    // signature lines (`log.showSignature`) run gpg and break --format output.
    cmd.args(["-c", "core.fsmonitor=false", "-c", "log.showSignature=false"]);
    if !trusted {
        for setting in UNTRUSTED_CONFIG {
            cmd.args(["-c", setting]);
        }
    }
    if let Some((subcommand, rest)) = args.split_first() {
        cmd.arg(subcommand);
        // Right after the subcommand: after `--` it would be a path.
        if !trusted && *subcommand == "diff" {
            cmd.arg("--no-ext-diff");
        }
        cmd.args(rest);
    }
    cmd.current_dir(root)
        // What git runs needs the PATH a terminal has, which an app started
        // from the Dock lacks: hooks (husky, npx), git-lfs, and credential
        // helpers from Homebrew or nvm.
        .env("PATH", crate::lsp::servers::search_path())
        // Never block on an interactive prompt: fail fast instead.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // `git status` would otherwise refresh .git/index, which the project
        // watcher sees as a change and answers with another status.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    // The repository is `root`. Inherited from a git hook or a shell, these
    // would point every command at another repository (a test run once
    // turned the project's own into a bare one this way).
    for var in REPOSITORY_VARS {
        cmd.env_remove(var);
    }
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

/// Runs git and returns stdout, or the most useful line of stderr.
pub fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    run_bytes(root, args).map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// Runs git and returns stdout as it is (file contents in any encoding).
fn run_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = command(root, args)
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(error_text(&output.stderr, &output.stdout))
    }
}

fn run_with_stdin(root: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    let mut child = command(root, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input).map_err(|e| e.to_string())?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(error_text(&output.stderr, &output.stdout))
    }
}

fn error_text(stderr: &[u8], stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(if stderr.is_empty() { stdout } else { stderr }).into_owned();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:"))
        .collect();
    let line = lines
        .iter()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or(lines.first())
        .copied()
        .unwrap_or("git failed");
    let mut message = line.trim_start_matches("fatal: ").trim_start_matches("error: ").to_string();
    // "…would be overwritten by merge:" is followed by the indented files.
    if message.ends_with(':') {
        let listed: Vec<&str> = text
            .lines()
            .skip_while(|l| l.trim() != line)
            .skip(1)
            .take_while(|l| l.starts_with([' ', '\t']))
            .map(str::trim)
            .collect();
        if !listed.is_empty() {
            message.push(' ');
            message.push_str(&listed.join(", "));
        }
    }
    message
}

pub fn is_available() -> bool {
    crate::process::command("git")
        .arg("--version")
        .env("PATH", crate::lsp::servers::search_path())
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The working-tree root of the repository containing `path`.
pub fn discover(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    let out = run(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let root = PathBuf::from(out.trim());
    Some(crate::paths::canonicalize(&root).unwrap_or(root))
}

pub fn init(dir: &Path) -> Result<(), String> {
    run(dir, &["init"]).map(drop)
}

pub fn status(root: &Path) -> Result<RepoStatus, String> {
    let out = run(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            // An ignored folder once, not every file in it (as `traditional` would).
            "--ignored=matching",
        ],
    )?;
    let mut status = parse_status(root, &out);
    status.state = repo_state(root);
    Ok(status)
}

/// The status of some paths only (relative to `root`; a folder with what's
/// in it), for `RepoStatus::merge`: what changed in the work tree there,
/// without reading the rest of the repository. Without the branch and
/// merge state, which only change with `.git` (a full `status` reads them).
pub fn status_of(root: &Path, rels: &[String]) -> Result<RepoStatus, String> {
    // No paths would be every path.
    if rels.is_empty() {
        return Ok(RepoStatus::default());
    }
    let mut args = vec!["--no-optional-locks", "--literal-pathspecs", "status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignored=matching", "--"];
    args.extend(rels.iter().map(String::as_str));
    Ok(parse_status(root, &run(root, &args)?))
}

/// The pathspecs for `status_of` that cover `paths` (absolute; those
/// outside `root` are left out): relative to the repository, none inside
/// another. A path in a nested repository (a submodule) is its folder,
/// which Git reports as one entry; the paths in it would read as nothing.
/// Past a few dozen, the deepest go to their folders, until they're few
/// enough (and a short command line). None when that comes to the whole
/// repository: read all of it. Blocking (it looks for `.git` folders).
pub fn pathspecs<'a>(root: &Path, paths: impl IntoIterator<Item = &'a Path>) -> Option<Vec<String>> {
    const MAX_PATHSPECS: usize = 64;
    // Windows takes a command line of 32K characters.
    const MAX_LENGTH: usize = 16_000;
    let mut nested: HashMap<PathBuf, bool> = HashMap::new();
    let mut specs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for path in paths {
        let Ok(rel) = path.strip_prefix(root) else { continue };
        // The outermost repository it's in, below this one.
        let mut folders: Vec<&Path> = rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).collect();
        folders.reverse();
        let repo = folders.into_iter().find(|folder| *nested.entry(folder.to_path_buf()).or_insert_with(|| root.join(folder).join(".git").exists()));
        specs.insert(repo.unwrap_or(rel).to_path_buf());
    }
    loop {
        // Sorted, a folder comes right before what's in it.
        let mut kept: Vec<PathBuf> = Vec::new();
        for spec in std::mem::take(&mut specs) {
            if !kept.last().is_some_and(|folder| spec.starts_with(folder)) {
                kept.push(spec);
            }
        }
        if kept.iter().any(|spec| spec.as_os_str().is_empty()) {
            return None;
        }
        let length: usize = kept.iter().map(|spec| spec.as_os_str().len() + 1).sum();
        if kept.len() <= MAX_PATHSPECS && length <= MAX_LENGTH {
            return Some(kept.iter().map(|spec| spec.to_string_lossy().replace('\\', "/")).collect());
        }
        let deepest = kept.iter().map(|spec| spec.components().count()).max().unwrap_or(0);
        specs = kept
            .into_iter()
            .map(|spec| if spec.components().count() == deepest { spec.parent().map(Path::to_path_buf).unwrap_or_default() } else { spec })
            .collect();
    }
}

impl RepoStatus {
    /// Takes in `scoped`, the status of `rels` alone (`status_of`): what was
    /// known under those paths is replaced, the rest kept. Staged changes
    /// stay as they were known: only `.git` changes (a full status) move
    /// them, and a rename seen from one side reads as an addition or a
    /// deletion. So a tracked file keeps its staged side and blob ids, and
    /// a staged change not known before is left out. `root` is the
    /// repository's.
    pub fn merge(&mut self, root: &Path, rels: &[String], scoped: RepoStatus) {
        let specs: Vec<PathBuf> = rels.iter().map(|rel| root.join(rel)).collect();
        let covered = |path: &Path| specs.iter().any(|spec| path.starts_with(spec));
        let mut before: HashMap<PathBuf, FileStatus> = HashMap::new();
        self.files.retain(|f| {
            let keep = !covered(&f.path);
            if !keep {
                before.insert(f.path.clone(), f.clone());
            }
            keep
        });
        for mut file in scoped.files {
            match before.get(&file.path) {
                Some(was) if was.blobs.is_some() && file.blobs.is_some() => {
                    file.staged = was.staged;
                    file.blobs = was.blobs.clone();
                }
                _ if file.staged.is_some() && file.unstaged.is_none() => continue,
                _ => {}
            }
            self.files.push(file);
        }
        self.files.sort_by(|a, b| a.rel.cmp(&b.rel));
        // A path in an ignored folder reports the folder.
        self.ignored.retain(|p| !covered(p));
        for path in scoped.ignored {
            if !self.ignored.contains(&path) {
                self.ignored.push(path);
            }
        }
    }
}

fn change_for(c: char) -> Option<Change> {
    match c {
        'M' | 'T' => Some(Change::Modified),
        'A' | 'C' => Some(Change::Added),
        'D' => Some(Change::Deleted),
        'R' => Some(Change::Renamed),
        'U' => Some(Change::Conflicted),
        _ => None,
    }
}

fn parse_status(root: &Path, out: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let file = |rel: &str, staged, unstaged, blobs: Option<String>| FileStatus {
        path: root.join(rel),
        rel: rel.to_string(),
        staged,
        unstaged,
        blobs,
    };
    while let Some(entry) = fields.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.head_oid = Some(value.to_string()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let kind = entry.chars().next().unwrap_or(' ');
        match kind {
            '1' | '2' => {
                // "1 XY sub mH mI mW hH hI path" ("2" adds a score before the path).
                let n = if kind == '1' { 8 } else { 9 };
                let parts: Vec<&str> = entry.splitn(n + 1, ' ').collect();
                if parts.len() <= n {
                    continue;
                }
                let xy: Vec<char> = parts[1].chars().collect();
                status.files.push(file(parts[n], change_for(xy[0]), change_for(xy[1]), Some(parts[6..8].join(" "))));
                if kind == '2' {
                    fields.next(); // the original path of a rename
                }
            }
            'u' => {
                let parts: Vec<&str> = entry.splitn(11, ' ').collect();
                if let Some(rel) = parts.get(10) {
                    status.files.push(file(rel, Some(Change::Conflicted), Some(Change::Conflicted), Some(parts[7..10].join(" "))));
                }
            }
            '?' => {
                status.files.push(file(&entry[2..], None, Some(Change::Untracked), None));
            }
            // A folder ends with a slash.
            '!' => status.ignored.push(root.join(entry[2..].trim_end_matches('/'))),
            _ => {}
        }
    }
    status.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    status
}

/// A file's contents at HEAD, or `None` if it isn't in HEAD.
pub fn head_text(root: &Path, rel: &str) -> Option<String> {
    run(root, &["show", &format!("HEAD:{rel}")]).ok()
}

/// A file's contents in the index (staging area).
pub fn index_text(root: &Path, rel: &str) -> Option<String> {
    run(root, &["show", &format!(":{rel}")]).ok()
}

/// A file's bytes at HEAD.
pub fn head_blob(root: &Path, rel: &str) -> Option<Vec<u8>> {
    run_bytes(root, &["show", &format!("HEAD:{rel}")]).ok()
}

/// A file's bytes in the index.
pub fn index_blob(root: &Path, rel: &str) -> Option<Vec<u8>> {
    run_bytes(root, &["show", &format!(":{rel}")]).ok()
}

/// Several objects' bytes from one `git cat-file --batch` (open files'
/// diff bases load this way), e.g. `HEAD:src/a.rs` and `:src/a.rs` (the
/// index): one process however many files. None for what doesn't exist.
pub fn read_blobs(root: &Path, specs: &[String]) -> Result<Vec<Option<Vec<u8>>>, String> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = command(root, &["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    // Written on another thread: git answers as it reads, and a long list
    // could fill both pipes.
    let input: String = specs.iter().map(|s| if s.contains('\n') { "\n".to_string() } else { format!("{s}\n") }).collect();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    parse_batch(&output.stdout, specs.len())
}

/// `cat-file --batch` output: per object `<oid> <type> <size>\n<bytes>\n`, or
/// `<spec> missing\n` (also for an empty line).
fn parse_batch(mut out: &[u8], count: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut blobs = Vec::with_capacity(count);
    while blobs.len() < count {
        let end = out.iter().position(|&b| b == b'\n').ok_or("git cat-file stopped early")?;
        let header = String::from_utf8_lossy(&out[..end]).into_owned();
        out = &out[end + 1..];
        let mut words = header.rsplitn(3, ' ');
        match (words.next().and_then(|n| n.parse::<usize>().ok()), words.next()) {
            (Some(size), Some(_kind)) if out.len() > size => {
                blobs.push(Some(out[..size].to_vec()));
                out = &out[size + 1..];
            }
            _ => blobs.push(None),
        }
    }
    Ok(blobs)
}

/// A file's text as an editor holds it (decoded, LF line ends), and how to
/// write text back the same way.
pub struct BlobText {
    pub text: String,
    encoding: TextEncoding,
    crlf: bool,
}

impl BlobText {
    /// Decodes `bytes`; `hint` is the encoding the editor uses for the file.
    pub fn new(bytes: &[u8], hint: Option<TextEncoding>) -> Self {
        let (text, encoding) = crate::encoding::decode(bytes, hint);
        let crlf = text.contains("\r\n");
        BlobText { text: crate::encoding::to_lf(text), encoding, crlf }
    }

    /// `text` (with LF line ends) in this blob's line ends and encoding.
    fn encode(&self, text: &str) -> Result<Vec<u8>, String> {
        let text = if self.crlf { text.replace('\n', "\r\n") } else { text.to_string() };
        crate::encoding::encode(&text, self.encoding)
    }
}

/// The index version of `rel` to stage lines into; a file new to the index
/// takes the working copy's line ends and encoding.
fn index_blob_text(root: &Path, rel: &str) -> BlobText {
    match index_blob(root, rel) {
        Some(bytes) => BlobText::new(&bytes, None),
        None => {
            let style = BlobText::new(&std::fs::read(root.join(rel)).unwrap_or_default(), None);
            BlobText { text: String::new(), ..style }
        }
    }
}

pub fn stage(root: &Path, rels: &[String]) -> Result<(), String> {
    let mut args = vec!["--literal-pathspecs", "add", "--all", "--"];
    args.extend(rels.iter().map(String::as_str));
    run(root, &args).map(drop)
}

/// Stages every change to tracked files, deletions included, but no
/// untracked file (as `commit --all` would).
pub fn stage_tracked(root: &Path) -> Result<(), String> {
    run(root, &["add", "--update"]).map(drop)
}

pub fn unstage(root: &Path, rels: &[String], has_head: bool) -> Result<(), String> {
    let mut args: Vec<&str> = if has_head {
        vec!["--literal-pathspecs", "restore", "--staged", "--"]
    } else {
        // Before the first commit there is nothing to restore from.
        vec!["--literal-pathspecs", "rm", "--cached", "-r", "--quiet", "--"]
    };
    args.extend(rels.iter().map(String::as_str));
    run(root, &args).map(drop)
}

/// Throws away working-tree changes. Untracked files go to the Trash.
pub fn discard(root: &Path, files: &[FileStatus]) -> Result<(), String> {
    discard_with(root, files, |path| crate::file_history::move_to_trash(path).map(drop))
}

/// `discard`, with `trash` taking untracked files away (tests keep them out
/// of the user's Trash).
fn discard_with(root: &Path, files: &[FileStatus], trash: impl Fn(&Path) -> std::io::Result<()>) -> Result<(), String> {
    let (untracked, tracked): (Vec<&FileStatus>, Vec<&FileStatus>) = files
        .iter()
        .partition(|f| f.unstaged == Some(Change::Untracked));
    for f in untracked {
        trash(&f.path).map_err(|e| format!("{}: {e}", f.rel))?;
    }
    if !tracked.is_empty() {
        let mut args = vec!["--literal-pathspecs", "restore", "--worktree", "--"];
        args.extend(tracked.iter().map(|f| f.rel.as_str()));
        run(root, &args)?;
    }
    Ok(())
}

pub fn commit(root: &Path, message: &str, amend: bool) -> Result<String, String> {
    let mut args = vec!["commit", "--quiet", "--file", "-"];
    if amend {
        args.push("--amend");
    }
    run_with_stdin(root, &args, message.as_bytes())?;
    Ok(run(root, &["log", "-1", "--format=%h %s"])?.trim().to_string())
}

pub fn last_commit_message(root: &Path) -> Option<String> {
    run(root, &["log", "-1", "--format=%B"]).ok().map(|s| s.trim_end().to_string())
}

/// `force` replaces the remote branch, but only if it's still where we last
/// saw it (`--force-with-lease`), so nobody's new commits are lost unseen.
/// Pushes the current branch; `set_upstream` (remote, branch) publishes a
/// branch that has no upstream yet.
pub fn push(root: &Path, set_upstream: Option<(&str, &str)>, force: bool) -> Result<String, String> {
    let mut args = vec!["push", "--porcelain"];
    if force {
        args.push("--force-with-lease");
    }
    if let Some((remote, branch)) = set_upstream {
        args.extend(["--set-upstream", remote, branch]);
    }
    run(root, &args)
}

/// What a push would send: the branch, where it goes, and its new commits.
#[derive(Clone, Debug)]
pub struct PushPreview {
    pub branch: String,
    /// "origin/main", or `None` when the branch has no upstream yet (the
    /// push creates `origin/<branch>`).
    pub upstream: Option<String>,
    pub commits: Vec<LogEntry>,
    /// Commits on the remote branch that aren't here (a push would be
    /// rejected without force).
    pub behind: u32,
    pub has_remote: bool,
    /// Every remote, for choosing where a branch without an upstream goes.
    pub remotes: Vec<String>,
}

pub fn push_preview(root: &Path) -> Result<PushPreview, String> {
    let status = status(root)?;
    let branch = status.branch.clone().ok_or("HEAD is detached; check out a branch to push")?;
    // Without an upstream, everything no remote has yet.
    let commits = match &status.upstream {
        Some(upstream) => log_args(root, 500, &[&format!("{upstream}..HEAD")])?,
        None => log_args(root, 500, &["HEAD", "--not", "--remotes"])?,
    };
    let remotes: Vec<String> = remotes(root).unwrap_or_default().into_iter().map(|r| r.name).collect();
    Ok(PushPreview { branch, upstream: status.upstream, commits, behind: status.behind, has_remote: !remotes.is_empty(), remotes })
}

/// How a pull combines incoming commits with local ones once both branches
/// have new commits (`pull_strategy` in settings).
#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PullStrategy {
    /// A merge commit joins them.
    Merge,
    /// Local commits are replayed on top of the incoming ones.
    Rebase,
    /// Only fast-forwards: a branch with commits of its own isn't pulled.
    FfOnly,
}

impl PullStrategy {
    pub const ALL: [PullStrategy; 3] = [PullStrategy::Merge, PullStrategy::Rebase, PullStrategy::FfOnly];

    /// The value in `settings.json`.
    pub fn key(self) -> &'static str {
        match self {
            PullStrategy::Merge => "merge",
            PullStrategy::Rebase => "rebase",
            PullStrategy::FfOnly => "ff_only",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PullStrategy::Merge => "Merge",
            PullStrategy::Rebase => "Rebase",
            PullStrategy::FfOnly => "Fast-forward only",
        }
    }

    /// `git pull`'s options for it; they override the user's Git config.
    fn args(self) -> &'static [&'static str] {
        match self {
            // `--ff`: a fast-forward when there's nothing local, else a merge commit.
            PullStrategy::Merge => &["--no-rebase", "--ff", "--no-edit"],
            PullStrategy::Rebase => &["--rebase"],
            PullStrategy::FfOnly => &["--ff-only"],
        }
    }
}

/// What a pull did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pulled {
    UpToDate,
    /// This many commits came in: fast-forwarded to, merged with the local
    /// ones, or with the local ones rebased onto them.
    FastForwarded(usize),
    Merged(usize),
    Rebased(usize),
    /// A merge or rebase stopped on conflicts (Continue or Abort it).
    Conflicts,
    /// Both branches have commits the other doesn't, and the pull only
    /// fast-forwards: nothing changed.
    Diverged { branch: String, upstream: String, ahead: u32, behind: u32 },
}

/// Whether the user's Git config says how a pull combines diverged
/// branches (`pull.rebase`, `pull.ff` or the branch's own `rebase`).
pub fn pull_configured(root: &Path, branch: Option<&str>) -> bool {
    let set = |key: &str| run(root, &["config", "--get", key]).is_ok_and(|value| !value.trim().is_empty());
    set("pull.rebase") || set("pull.ff") || branch.is_some_and(|b| set(&format!("branch.{b}.rebase")))
}

/// Pulls the current branch's upstream. Without a `strategy`, the user's Git
/// config decides if it says how; otherwise only a fast-forward happens, and
/// branches that both have new commits come back as `Diverged`, so nobody gets
/// a merge commit they didn't choose.
pub fn pull(root: &Path, strategy: Option<PullStrategy>) -> Result<Pulled, String> {
    let head = || run(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).ok().map(|s| s.trim().to_string());
    let count = |range: &str| run(root, &["rev-list", "--count", range]).ok().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
    let before = head();
    let args: &[&str] = match strategy {
        Some(strategy) => strategy.args(),
        None if pull_configured(root, status(root).ok().and_then(|s| s.branch).as_deref()) => &["--no-edit"],
        None => PullStrategy::FfOnly.args(),
    };
    if let Err(err) = run(root, &[["pull"].as_slice(), args].concat()) {
        if conflicts_or(root, err.clone()).is_ok() {
            return Ok(Pulled::Conflicts);
        }
        // A fast-forward-only pull fetched before it gave up, so this
        // compares with what came in.
        return match status(root) {
            Ok(s) if args == PullStrategy::FfOnly.args() && s.ahead > 0 && s.behind > 0 => Ok(Pulled::Diverged {
                branch: s.branch.unwrap_or_default(),
                upstream: s.upstream.unwrap_or_default(),
                ahead: s.ahead,
                behind: s.behind,
            }),
            _ => Err(err),
        };
    }
    let Some(after) = head().filter(|after| before.as_ref() != Some(after)) else { return Ok(Pulled::UpToDate) };
    // A branch without commits of its own gets everything.
    let Some(before) = before else { return Ok(Pulled::FastForwarded(count("HEAD"))) };
    let incoming = count(&format!("{before}..@{{upstream}}"));
    let parents = run(root, &["rev-list", "--parents", "-n", "1", "HEAD"]).unwrap_or_default();
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    Ok(if parents.len() > 1 && parents.contains(&before.as_str()) {
        Pulled::Merged(incoming)
    } else if run(root, &["merge-base", "--is-ancestor", &before, &after]).is_ok() {
        Pulled::FastForwarded(incoming)
    } else {
        Pulled::Rebased(incoming)
    })
}

pub fn fetch(root: &Path) -> Result<String, String> {
    run(root, &["fetch", "--all", "--prune"])
}

pub fn branches(root: &Path) -> Result<Vec<Branch>, String> {
    let out = run(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(HEAD)\t%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (head, refname) = line.split_once('\t')?;
            let (name, remote) = if let Some(n) = refname.strip_prefix("refs/heads/") {
                (n, false)
            } else {
                (refname.strip_prefix("refs/remotes/")?, true)
            };
            if remote && name.ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                name: name.to_string(),
                remote,
                current: head == "*",
            })
        })
        .collect())
}

pub fn checkout(root: &Path, branch: &Branch) -> Result<(), String> {
    if branch.remote {
        // Check out a local tracking branch for the remote one.
        let local = branch.name.split_once('/').map_or(branch.name.as_str(), |(_, n)| n);
        run(root, &["switch", "--track", "-c", local, &branch.name])
            .or_else(|_| run(root, &["switch", local]))
            .map(drop)
    } else {
        run(root, &["switch", &branch.name]).map(drop)
    }
}

pub fn create_branch(root: &Path, name: &str) -> Result<(), String> {
    run(root, &["switch", "-c", name]).map(drop)
}

/// The URL of `origin`, or of the first remote if there's no `origin`.
pub fn remote_url(root: &Path) -> Option<String> {
    let url = run(root, &["remote", "get-url", "origin"]).ok().or_else(|| {
        let first = run(root, &["remote"]).ok()?.lines().next()?.trim().to_string();
        run(root, &["remote", "get-url", &first]).ok()
    })?;
    Some(url.trim().to_string()).filter(|u| !u.is_empty())
}

/// A browsable `https://` page for a remote URL such as
/// `git@github.com:owner/repo.git` or `ssh://git@host:22/owner/repo`.
pub fn web_url(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let (host, path) = if let Some(rest) = remote.strip_prefix("https://").or_else(|| remote.strip_prefix("http://")) {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        rest.split_once('/')?
    } else if let Some(rest) = remote.strip_prefix("ssh://").or_else(|| remote.strip_prefix("git://")) {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?, path)
    } else {
        // scp-like: [user@]host:path
        let rest = remote.rsplit_once('@').map_or(remote, |(_, r)| r);
        let (host, path) = rest.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        (host, path)
    };
    (!host.is_empty() && !path.is_empty()).then(|| format!("https://{host}/{}", path.trim_start_matches('/')))
}

/// The folder name `git clone` would pick for a URL ("repo" for
/// `https://host/owner/repo.git`).
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(['/', '\\']);
    let url = url.strip_suffix(".git").unwrap_or(url);
    let name = url.rsplit(['/', '\\', ':']).next()?.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
}

/// The commit `HEAD` points at, if there is one.
pub fn last_commit(root: &Path) -> Option<CommitInfo> {
    let out = run(root, &["log", "-1", "--format=%h%x1f%s%x1f%an%x1f%ct"]).ok()?;
    let mut parts = out.trim_end().splitn(4, '\x1f');
    Some(CommitInfo {
        sha: parts.next()?.to_string(),
        summary: parts.next()?.to_string(),
        author: parts.next()?.to_string(),
        time: parts.next()?.parse().ok()?,
    })
}

/// A running `git clone`. Progress is read from its stderr; `cancel` stops it.
pub struct CloneJob {
    child: std::sync::Mutex<std::process::Child>,
    stderr: std::sync::Mutex<Option<std::process::ChildStderr>>,
    /// The latest progress line, e.g. "Receiving objects:  45% (450/1000)".
    pub progress: std::sync::Mutex<String>,
    cancelled: std::sync::atomic::AtomicBool,
    dest: PathBuf,
}

/// Starts cloning `url` into `parent/name`. `shallow` fetches only the latest commit.
pub fn start_clone(parent: &Path, url: &str, name: &str, shallow: bool) -> Result<CloneJob, String> {
    let mut args = vec!["clone", "--progress"];
    if shallow {
        args.extend(["--depth", "1"]);
    }
    args.extend(["--", url, name]);
    let mut child = command(parent, &args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    let stderr = child.stderr.take();
    Ok(CloneJob {
        child: std::sync::Mutex::new(child),
        stderr: std::sync::Mutex::new(stderr),
        progress: std::sync::Mutex::new("Starting…".into()),
        cancelled: std::sync::atomic::AtomicBool::new(false),
        dest: parent.join(name),
    })
}

impl CloneJob {
    /// Blocks until git exits, updating `progress` along the way.
    pub fn wait(&self) -> Result<PathBuf, String> {
        let mut all = Vec::new();
        if let Some(mut stderr) = self.stderr.lock().unwrap().take() {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                // Progress lines are redrawn with '\r'; keep the newest one.
                let text = String::from_utf8_lossy(&all[all.len().saturating_sub(512)..]).into_owned();
                if let Some(line) = text.rsplit(['\r', '\n']).map(str::trim).find(|l| !l.is_empty()) {
                    *self.progress.lock().unwrap() = line.trim_start_matches("remote: ").to_string();
                }
            }
        }
        let status = self.child.lock().unwrap().wait().map_err(|e| e.to_string())?;
        if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            // Killed, so git couldn't clean up after itself. Callers only
            // clone into folders that didn't exist, so this is all ours.
            std::fs::remove_dir_all(&self.dest).ok();
            Err("Cancelled".into())
        } else if status.success() {
            Ok(self.dest.clone())
        } else {
            Err(error_text(&all, &[]))
        }
    }

    /// Stops the clone; `wait` then removes the half-written folder.
    pub fn cancel(&self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut child) = self.child.try_lock() {
            child.kill().ok();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    pub author: String,
    pub time: i64,
    pub summary: String,
    pub sha: String,
    pub uncommitted: bool,
}

impl BlameLine {
    /// A line not committed yet.
    pub fn uncommitted() -> Self {
        BlameLine { author: String::new(), time: 0, summary: String::new(), sha: String::new(), uncommitted: true }
    }
}

/// A whole file's blame (`blame_file`): which commit each row came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileBlame {
    /// Per row of the text blamed: its commit in `commits` (`u32::MAX`: git
    /// didn't say).
    rows: Vec<u32>,
    commits: Vec<BlameLine>,
}

impl FileBlame {
    /// The commit that row `row` (0-based) of the text blamed came from.
    pub fn line(&self, row: usize) -> Option<&BlameLine> {
        self.commits.get(*self.rows.get(row)? as usize)
    }

    /// A blame of rows from these commits (tests).
    #[cfg(test)]
    pub fn of(commits: Vec<BlameLine>, rows: Vec<u32>) -> Self {
        FileBlame { rows, commits }
    }

    /// Reads `git blame --incremental`: per run of rows, the commit (with
    /// its author, time and summary the first time it's named) and where
    /// the rows are, ending with its `filename` line.
    fn parse(out: &str) -> Self {
        let mut blame = FileBlame::default();
        let mut by_sha: HashMap<&str, u32> = HashMap::new();
        let mut lines = out.lines();
        while let Some(header) = lines.next() {
            let mut fields = header.split_whitespace();
            let (Some(sha), Some(_), Some(row), Some(count)) = (fields.next(), fields.next(), fields.next(), fields.next()) else { continue };
            let (Ok(row), Ok(count)) = (row.parse::<usize>(), count.parse::<usize>()) else { continue };
            let ix = *by_sha.entry(sha).or_insert_with(|| {
                blame.commits.push(BlameLine {
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                    sha: sha.chars().take(8).collect(),
                    uncommitted: sha.chars().all(|c| c == '0'),
                });
                (blame.commits.len() - 1) as u32
            });
            let commit = &mut blame.commits[ix as usize];
            for line in lines.by_ref() {
                if let Some(author) = line.strip_prefix("author ") {
                    commit.author = author.to_string();
                } else if let Some(time) = line.strip_prefix("author-time ") {
                    commit.time = time.parse().unwrap_or(0);
                } else if let Some(summary) = line.strip_prefix("summary ") {
                    commit.summary = summary.to_string();
                } else if line.starts_with("filename ") {
                    break;
                }
            }
            // 1-based in git's output.
            let rows = row.saturating_sub(1)..row.saturating_sub(1) + count;
            if blame.rows.len() < rows.end {
                blame.rows.resize(rows.end, u32::MAX);
            }
            blame.rows[rows].fill(ix);
        }
        blame
    }
}

/// Blame for every line of `rel`, computed against the given buffer text
/// so unsaved edits are accounted for (as not committed). The `stop`'s
/// owner dropping it kills git.
pub fn blame_file(root: &Path, rel: &str, contents: &[u8], stop: &StopToken) -> Result<FileBlame, String> {
    let out = run_stoppable(root, &["blame", "--incremental", "--contents", "-", "--", rel], contents, stop)?;
    Ok(FileBlame::parse(&out))
}

/// Kills the git command run with one of its tokens (`token`) when dropped:
/// work that newer work replaced doesn't keep git running.
#[derive(Default)]
pub struct KillOnDrop(Arc<Mutex<Running>>);

/// What a git command run with a `KillOnDrop`'s token is: once the owner
/// went, it's killed, or not started.
pub struct StopToken(Arc<Mutex<Running>>);

#[derive(Default)]
struct Running {
    dropped: bool,
    child: Option<Child>,
}

impl KillOnDrop {
    pub fn token(&self) -> StopToken {
        StopToken(self.0.clone())
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        running.dropped = true;
        if let Some(child) = &mut running.child {
            child.kill().ok();
        }
    }
}

/// `run_with_stdin`, killed when `stop`'s owner is dropped.
fn run_stoppable(root: &Path, args: &[&str], input: &[u8], stop: &StopToken) -> Result<String, String> {
    let mut child = command(root, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    {
        let mut running = stop.0.lock().unwrap_or_else(PoisonError::into_inner);
        if running.dropped {
            child.kill().ok();
            child.wait().ok();
            return Err("stopped".into());
        }
        running.child = Some(child);
    }
    // The input first (blame reads it all before it answers), then what it says.
    let written = stdin.map_or(Ok(()), |mut stdin| stdin.write_all(input));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    if let Some(mut stdout) = stdout {
        stdout.read_to_end(&mut out).ok();
    }
    if let Some(mut stderr) = stderr {
        stderr.read_to_end(&mut err).ok();
    }
    let child = stop.0.lock().unwrap_or_else(PoisonError::into_inner).child.take();
    let status = child.map(|mut child| child.wait()).transpose().map_err(|e| e.to_string())?;
    match status {
        Some(status) if status.success() && written.is_ok() => Ok(String::from_utf8_lossy(&out).into_owned()),
        Some(status) if !status.success() && !err.is_empty() => Err(error_text(&err, &out)),
        _ => Err("stopped".into()),
    }
}

pub fn user_name(root: &Path) -> Option<String> {
    run(root, &["config", "user.name"]).ok().map(|s| s.trim().to_string())
}

/// "3 days ago" style relative time.
pub fn relative_time(unix: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs() as i64;
    let secs = (now - unix).max(0);
    let (n, unit) = match secs {
        0..60 => return "just now".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86400 => (secs / 3600, "hour"),
        86400..2_592_000 => (secs / 86400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

// ---- Line diffs ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Added,
    Modified,
    Deleted,
}

/// A changed region between the base text and the current buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub kind: HunkKind,
    /// Rows in the current buffer (empty for deletions: the rows were
    /// removed just before `new_rows.start`).
    pub new_rows: Range<usize>,
    /// Rows in the base text.
    pub old_rows: Range<usize>,
    /// The base text of `old_rows`, including trailing newlines.
    pub old_text: String,
}

/// Lines as git counts them: split after each `\n` only. (`similar`'s own
/// line diff also splits at a lone `\r`, which binary files and old Mac
/// text are full of, so its line numbers wouldn't match these.)
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Git's test for binary content: a NUL byte near the start.
pub fn is_binary(text: &str) -> bool {
    let end = text.len().min(8000);
    text.as_bytes()[..end].contains(&0)
}

pub fn diff_hunks(base: &str, current: &str) -> Vec<Hunk> {
    use similar::DiffOp;
    let old_lines = split_lines(base);
    let new_lines = split_lines(current);
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(200))
        .diff_slices(&old_lines, &new_lines);
    let text = |r: Range<usize>| old_lines[r.start.min(old_lines.len())..r.end.min(old_lines.len())].concat();
    diff.ops()
        .iter()
        .filter_map(|op| match *op {
            DiffOp::Equal { .. } => None,
            DiffOp::Delete { old_index, old_len, new_index } => Some(Hunk {
                kind: HunkKind::Deleted,
                new_rows: new_index..new_index,
                old_rows: old_index..old_index + old_len,
                old_text: text(old_index..old_index + old_len),
            }),
            DiffOp::Insert { old_index, new_index, new_len } => Some(Hunk {
                kind: HunkKind::Added,
                new_rows: new_index..new_index + new_len,
                old_rows: old_index..old_index,
                old_text: String::new(),
            }),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => Some(Hunk {
                kind: HunkKind::Modified,
                new_rows: new_index..new_index + new_len,
                old_rows: old_index..old_index + old_len,
                old_text: text(old_index..old_index + old_len),
            }),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    /// A run of unchanged lines that was collapsed.
    Skipped,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// The line with tabs expanded (the skipped-lines label for `Skipped`).
    pub text: String,
    /// Byte ranges of `text` that changed within a modified line.
    pub emphasis: Vec<Range<usize>>,
    /// The change block this line belongs to, and whether it's its first line.
    pub change: Option<(usize, bool)>,
}

/// One side of a side-by-side row.
#[derive(Clone, Debug)]
pub struct DiffSide {
    pub no: usize,
    pub text: String,
    pub emphasis: Vec<Range<usize>>,
}

#[derive(Clone, Debug)]
pub struct SplitRow {
    /// `Context`, `Skipped`, or `Added`/`Removed` for any row of a change.
    pub kind: DiffLineKind,
    pub left: Option<DiffSide>,
    pub right: Option<DiffSide>,
    /// The skipped-lines label.
    pub label: String,
    pub change: Option<(usize, bool)>,
}

/// A contiguous change: rows in the base and in the current text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffChange {
    pub old_rows: Range<usize>,
    pub new_rows: Range<usize>,
}

/// A diff in both layouts, with unchanged runs beyond `context` lines collapsed.
#[derive(Clone, Debug, Default)]
pub struct DiffDocument {
    pub unified: Vec<DiffLine>,
    pub split: Vec<SplitRow>,
    pub changes: Vec<DiffChange>,
    pub added: usize,
    pub removed: usize,
}

fn expand_tabs(line: &str) -> String {
    line.trim_end_matches(['\n', '\r']).replace('\t', "    ")
}

/// Splits a line into words, whitespace runs and single punctuation marks.
fn tokens(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| if c.is_alphanumeric() || c == '_' { 0 } else if c.is_whitespace() { 1 } else { 2 };
    let mut prev = None;
    for (i, c) in line.char_indices() {
        let k = class(c);
        if i > start && (prev != Some(k) || k == 2) {
            out.push(&line[start..i]);
            start = i;
        }
        prev = Some(k);
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

/// Changed byte ranges between two versions of a line, by token; empty when
/// the lines have little in common (the whole line reads as changed then).
fn word_emphasis(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (a, b) = (tokens(old), tokens(new));
    let diff = similar::TextDiff::configure().timeout(Duration::from_millis(20)).diff_slices(&a, &b);
    if diff.ratio() < 0.4 {
        return (Vec::new(), Vec::new());
    }
    // Byte offset of each token, plus the end.
    let offsets = |toks: &[&str]| -> Vec<usize> {
        let mut at = 0;
        let mut v: Vec<usize> = toks
            .iter()
            .map(|t| {
                let start = at;
                at += t.len();
                start
            })
            .collect();
        v.push(at);
        v
    };
    let (oa, ob) = (offsets(&a), offsets(&b));
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for op in diff.ops() {
        if !matches!(op, similar::DiffOp::Equal { .. }) {
            let (o, n) = (op.old_range(), op.new_range());
            if !o.is_empty() {
                left.push(oa[o.start]..oa[o.end]);
            }
            if !n.is_empty() {
                right.push(ob[n.start]..ob[n.end]);
            }
        }
    }
    (left, right)
}

pub fn diff_document(base: &str, current: &str, context: usize) -> DiffDocument {
    use similar::DiffOp;
    let (old_raw, new_raw) = (split_lines(base), split_lines(current));
    let diff = similar::TextDiff::configure().timeout(Duration::from_secs(2)).diff_slices(&old_raw, &new_raw);
    let old_lines: Vec<String> = old_raw.iter().map(|l| expand_tabs(l)).collect();
    let new_lines: Vec<String> = new_raw.iter().map(|l| expand_tabs(l)).collect();
    let mut doc = DiffDocument::default();
    let mut shown_until = 0; // old-line index after the last shown line
    let skipped = |n: usize| format!("{n} unchanged line{}", if n == 1 { "" } else { "s" });
    let push_skipped = |doc: &mut DiffDocument, n: usize| {
        doc.unified.push(DiffLine { kind: DiffLineKind::Skipped, old_no: None, new_no: None, text: skipped(n), emphasis: Vec::new(), change: None });
        doc.split.push(SplitRow { kind: DiffLineKind::Skipped, left: None, right: None, label: skipped(n), change: None });
    };
    let groups = if context == usize::MAX {
        vec![diff.ops().to_vec()]
    } else {
        diff.grouped_ops(context)
    };
    for group in groups {
        let start = group.first().map_or(0, |op| op.old_range().start);
        if start > shown_until {
            push_skipped(&mut doc, start - shown_until);
        }
        shown_until = group.last().map_or(start, |op| op.old_range().end);
        for op in &group {
            let (old, new) = (op.old_range(), op.new_range());
            let side = |lines: &[String], i: usize, emphasis: Vec<Range<usize>>| DiffSide { no: i + 1, text: lines[i].clone(), emphasis };
            if let DiffOp::Equal { .. } = op {
                for (o, n) in old.clone().zip(new.clone()) {
                    doc.unified.push(DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(o + 1),
                        new_no: Some(n + 1),
                        text: new_lines[n].clone(),
                        emphasis: Vec::new(),
                        change: None,
                    });
                    doc.split.push(SplitRow {
                        kind: DiffLineKind::Context,
                        left: Some(side(&old_lines, o, Vec::new())),
                        right: Some(side(&new_lines, n, Vec::new())),
                        label: String::new(),
                        change: None,
                    });
                }
                continue;
            }
            let ix = doc.changes.len();
            doc.changes.push(DiffChange { old_rows: old.clone(), new_rows: new.clone() });
            doc.added += new.len();
            doc.removed += old.len();
            // Pair lines of a replacement for word emphasis.
            let pairs = old.len().min(new.len());
            let emphasis: Vec<_> = (0..pairs).map(|k| word_emphasis(&old_lines[old.start + k], &new_lines[new.start + k])).collect();
            let mut first = true;
            for (k, o) in old.clone().enumerate() {
                doc.unified.push(DiffLine {
                    kind: DiffLineKind::Removed,
                    old_no: Some(o + 1),
                    new_no: None,
                    text: old_lines[o].clone(),
                    emphasis: emphasis.get(k).map(|e| e.0.clone()).unwrap_or_default(),
                    change: Some((ix, std::mem::take(&mut first))),
                });
            }
            for (k, n) in new.clone().enumerate() {
                doc.unified.push(DiffLine {
                    kind: DiffLineKind::Added,
                    old_no: None,
                    new_no: Some(n + 1),
                    text: new_lines[n].clone(),
                    emphasis: emphasis.get(k).map(|e| e.1.clone()).unwrap_or_default(),
                    change: Some((ix, std::mem::take(&mut first))),
                });
            }
            for k in 0..old.len().max(new.len()) {
                let left = (k < old.len()).then(|| side(&old_lines, old.start + k, emphasis.get(k).map(|e| e.0.clone()).unwrap_or_default()));
                let right = (k < new.len()).then(|| side(&new_lines, new.start + k, emphasis.get(k).map(|e| e.1.clone()).unwrap_or_default()));
                doc.split.push(SplitRow {
                    kind: if left.is_some() { DiffLineKind::Removed } else { DiffLineKind::Added },
                    left,
                    right,
                    label: String::new(),
                    change: Some((ix, k == 0)),
                });
            }
        }
    }
    if !doc.changes.is_empty() && old_lines.len() > shown_until {
        push_skipped(&mut doc, old_lines.len() - shown_until);
    }
    doc
}

// ---- Partial staging ----------------------------------------------------------------

/// Whether two row ranges touch. An empty range (a deletion point) touches
/// the rows on either side of it.
pub fn rows_touch(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() || b.is_empty() {
        a.start <= b.end && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// `base` with the hunks of `base → target` that `pick` selects applied.
pub fn apply_hunks(base: &str, target: &str, pick: impl Fn(&Hunk) -> bool) -> String {
    let base_lines = split_lines(base);
    let target_lines = split_lines(target);
    let mut out = String::with_capacity(base.len().max(target.len()));
    let mut at = 0;
    for hunk in diff_hunks(base, target).iter().filter(|h| pick(h)) {
        out.extend(base_lines[at..hunk.old_rows.start].iter().copied());
        out.extend(target_lines[hunk.new_rows.clone()].iter().copied());
        at = hunk.old_rows.end;
    }
    out.extend(base_lines[at..].iter().copied());
    out
}

/// Maps rows of `target` to rows of `base` across the changes between them
/// (rows inside a change map to the whole changed range).
pub fn map_rows_to_base(base: &str, target: &str, rows: Range<usize>) -> Range<usize> {
    let hunks = diff_hunks(base, target);
    let map = |row: usize, end: bool| {
        let mut delta: isize = 0;
        for h in &hunks {
            if h.new_rows.start >= row {
                break;
            }
            if h.new_rows.end <= row {
                delta += h.old_rows.len() as isize - h.new_rows.len() as isize;
            } else {
                // Inside a change: snap to its edge in the base.
                return if end { h.old_rows.end } else { h.old_rows.start };
            }
        }
        (row as isize + delta).max(0) as usize
    };
    let start = map(rows.start, false);
    start..map(rows.end, true).max(start)
}

/// Stages the changes in `current` (the working copy or an editor's text)
/// that touch `rows` of it, leaving the file's other changes unstaged.
pub fn stage_lines(root: &Path, rel: &str, current: &str, rows: Range<usize>) -> Result<(), String> {
    // Compared with LF line ends, as editors hold text; written back in the
    // index's own line ends and encoding, so only the picked lines change.
    let index = index_blob_text(root, rel);
    let staged = apply_hunks(&index.text, &crate::encoding::to_lf(current.to_string()), |h| rows_touch(&h.new_rows, &rows));
    write_index(root, rel, &index.encode(&staged)?)
}

/// Unstages the staged changes touching `rows` of the index version.
/// `current` maps rows of an editor's text to the index first.
pub fn unstage_lines(root: &Path, rel: &str, current: Option<&str>, rows: Range<usize>) -> Result<(), String> {
    let index = index_blob_text(root, rel);
    let rows = match current {
        Some(current) => map_rows_to_base(&index.text, &crate::encoding::to_lf(current.to_string()), rows),
        None => rows,
    };
    let Some(head) = head_blob(root, rel) else {
        // Not in HEAD: unstaging part of a new file isn't meaningful.
        return unstage(root, &[rel.to_string()], has_head(root));
    };
    let head = BlobText::new(&head, Some(index.encoding)).text;
    let unstaged = apply_hunks(&index.text, &head, |h| rows_touch(&h.old_rows, &rows));
    write_index(root, rel, &index.encode(&unstaged)?)
}

fn has_head(root: &Path) -> bool {
    run(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
}

/// Puts `contents` in the index as `rel`, keeping its file mode.
fn write_index(root: &Path, rel: &str, contents: &[u8]) -> Result<(), String> {
    let sha = run_with_stdin(root, &["hash-object", "-w", "--stdin", "--path", rel], contents)?;
    let mode = run(root, &["--literal-pathspecs", "ls-files", "--stage", "--", rel])
        .ok()
        .and_then(|out| out.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| {
            let executable = std::fs::metadata(root.join(rel)).is_ok_and(|m| is_executable(&m));
            if executable { "100755" } else { "100644" }.to_string()
        });
    let info = format!("{mode},{},{rel}", sha.trim());
    run(root, &["update-index", "--add", "--cacheinfo", &info]).map(drop)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

// ---- Merges and rebases ----------------------------------------------------------------

/// An operation in progress that stopped (usually on conflicts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoState {
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
}

impl RepoState {
    pub fn label(self) -> &'static str {
        match self {
            RepoState::Merging => "Merging",
            RepoState::Rebasing => "Rebasing",
            RepoState::CherryPicking => "Cherry-picking",
            RepoState::Reverting => "Reverting",
        }
    }

    /// "Merge", for "Merge aborted".
    pub fn noun(self) -> &'static str {
        match self {
            RepoState::Merging => "Merge",
            RepoState::Rebasing => "Rebase",
            RepoState::CherryPicking => "Cherry-pick",
            RepoState::Reverting => "Revert",
        }
    }

    fn command(self) -> &'static str {
        match self {
            RepoState::Merging => "merge",
            RepoState::Rebasing => "rebase",
            RepoState::CherryPicking => "cherry-pick",
            RepoState::Reverting => "revert",
        }
    }
}

/// The `.git` directory (worktrees and submodules point elsewhere).
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    let out = run(root, &["rev-parse", "--absolute-git-dir"]).ok()?;
    Some(PathBuf::from(out.trim()))
}

pub fn repo_state(root: &Path) -> Option<RepoState> {
    let dir = git_dir(root)?;
    if dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir() {
        Some(RepoState::Rebasing)
    } else if dir.join("MERGE_HEAD").is_file() {
        Some(RepoState::Merging)
    } else if dir.join("CHERRY_PICK_HEAD").is_file() {
        Some(RepoState::CherryPicking)
    } else if dir.join("REVERT_HEAD").is_file() {
        Some(RepoState::Reverting)
    } else {
        None
    }
}

/// The message git prepared for the merge commit (MERGE_MSG), without comments.
pub fn merge_message(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir(root)?.join("MERGE_MSG")).ok()?;
    let message: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    Some(message.join("\n").trim().to_string()).filter(|m| !m.is_empty())
}

pub fn abort(root: &Path, state: RepoState) -> Result<(), String> {
    run(root, &[state.command(), "--abort"]).map(drop)
}

/// Continues after conflicts are resolved and staged, keeping git's message.
pub fn continue_op(root: &Path, state: RepoState) -> Result<String, String> {
    match state {
        // A merge continues by committing.
        RepoState::Merging => run(root, &["commit", "--no-edit"]),
        _ => run(root, &["-c", "core.editor=true", state.command(), "--continue"]),
    }
}

/// Merges `branch` into the current branch. Conflicts aren't an error: the
/// merge stops and `repo_state` reports it.
pub fn merge(root: &Path, branch: &str) -> Result<String, String> {
    run(root, &["merge", "--no-edit", branch]).or_else(|e| conflicts_or(root, e))
}

pub fn rebase(root: &Path, onto: &str) -> Result<String, String> {
    run(root, &["-c", "core.editor=true", "rebase", onto]).or_else(|e| conflicts_or(root, e))
}

fn conflicts_or(root: &Path, err: String) -> Result<String, String> {
    if repo_state(root).is_some() && status(root).is_ok_and(|s| s.files.iter().any(|f| f.staged == Some(Change::Conflicted))) {
        Ok("CONFLICT".into())
    } else {
        Err(err)
    }
}

// ---- Remotes ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    /// Where it fetches from.
    pub url: String,
}

pub fn remotes(root: &Path) -> Result<Vec<Remote>, String> {
    let out = run(root, &["remote", "-v"])?;
    let mut remotes: Vec<Remote> = Vec::new();
    for line in out.lines().filter(|l| l.ends_with("(fetch)")) {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(url)) = (parts.next(), parts.next())
            && !remotes.iter().any(|r| r.name == name)
        {
            remotes.push(Remote { name: name.to_string(), url: url.to_string() });
        }
    }
    Ok(remotes)
}

pub fn add_remote(root: &Path, name: &str, url: &str) -> Result<(), String> {
    run(root, &["remote", "add", name, url]).map(drop)
}

pub fn remove_remote(root: &Path, name: &str) -> Result<(), String> {
    run(root, &["remote", "remove", name]).map(drop)
}

pub fn rename_remote(root: &Path, from: &str, to: &str) -> Result<(), String> {
    run(root, &["remote", "rename", from, to]).map(drop)
}

pub fn set_remote_url(root: &Path, name: &str, url: &str) -> Result<(), String> {
    run(root, &["remote", "set-url", name, url]).map(drop)
}

/// Where a branch without an upstream is pushed: `origin`, else the only or
/// first remote.
pub fn default_remote(root: &Path) -> Option<String> {
    let remotes = remotes(root).ok()?;
    remotes.iter().find(|r| r.name == "origin").or(remotes.first()).map(|r| r.name.clone())
}

// ---- Branches, tags and commits ----------------------------------------------------

pub fn delete_branch(root: &Path, name: &str, force: bool) -> Result<(), String> {
    run(root, &["branch", if force { "-D" } else { "-d" }, name]).map(drop)
}

pub fn rename_branch(root: &Path, from: &str, to: &str) -> Result<(), String> {
    run(root, &["branch", "-m", from, to]).map(drop)
}

/// Whether `branch`'s commits are all in HEAD (deleting it loses nothing).
pub fn is_merged(root: &Path, branch: &str) -> bool {
    run(root, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok()
}

/// A name Git accepts for a branch.
pub fn valid_branch_name(root: &Path, name: &str) -> bool {
    run(root, &["check-ref-format", "--branch", name]).is_ok()
}

pub fn checkout_commit(root: &Path, sha: &str) -> Result<(), String> {
    run(root, &["switch", "--detach", sha]).map(drop)
}

pub fn create_branch_at(root: &Path, name: &str, at: &str) -> Result<(), String> {
    run(root, &["switch", "-c", name, at]).map(drop)
}

pub fn create_tag(root: &Path, name: &str, at: &str) -> Result<(), String> {
    run(root, &["tag", name, at]).map(drop)
}

/// Like `merge`: conflicts stop it ("CONFLICT"), for Continue / Abort.
pub fn cherry_pick(root: &Path, sha: &str) -> Result<String, String> {
    run(root, &["cherry-pick", sha]).or_else(|e| conflicts_or(root, e))
}

pub fn revert(root: &Path, sha: &str) -> Result<String, String> {
    run(root, &["revert", "--no-edit", sha]).or_else(|e| conflicts_or(root, e))
}

/// How much a reset keeps: the changes staged (soft), in the working tree
/// (mixed), or nothing (hard).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

pub fn reset(root: &Path, to: &str, mode: ResetMode) -> Result<(), String> {
    run(root, &["reset", mode.flag(), to]).map(drop)
}

// ---- Stashes -----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    /// `stash@{index}`.
    pub index: usize,
    /// "On main: message", or "WIP on main: abc123 subject".
    pub message: String,
    pub time: i64,
}

pub fn stashes(root: &Path) -> Vec<Stash> {
    let Ok(out) = run(root, &["stash", "list", "--format=%gd%x00%gs%x00%ct"]) else { return Vec::new() };
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let index = parts.next()?.strip_prefix("stash@{")?.strip_suffix('}')?.parse().ok()?;
            let message = parts.next()?.to_string();
            let time = parts.next()?.parse().unwrap_or(0);
            Some(Stash { index, message, time })
        })
        .collect()
}

/// Stashes the changes to tracked files, and untracked files too when
/// `untracked`, with an optional message. `false` when there was nothing to
/// stash (Git says so but succeeds).
pub fn stash(root: &Path, message: Option<&str>, untracked: bool) -> Result<bool, String> {
    let top = || run(root, &["rev-parse", "--verify", "--quiet", "refs/stash"]).ok();
    let before = top();
    let mut args = vec!["stash", "push"];
    if untracked {
        args.push("--include-untracked");
    }
    if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
        args.extend(["--message", message]);
    }
    run(root, &args)?;
    Ok(top() != before)
}

/// The untracked files a stash holds that exist in the working tree again.
/// Git won't write over them, so it would apply the rest of the stash
/// without them and keep it.
pub fn stash_blockers(root: &Path, index: usize) -> Vec<String> {
    // The third parent of a stash commit holds its untracked files.
    let Ok(out) = run(root, &["ls-tree", "-r", "-z", "--name-only", &format!("stash@{{{index}}}^3")]) else {
        return Vec::new();
    };
    out.split('\0').filter(|rel| !rel.is_empty() && root.join(rel).symlink_metadata().is_ok()).map(str::to_string).collect()
}

/// What applying a stash did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unstashed {
    Applied,
    /// Stopped on conflicts: the files are left conflicted and the stash kept.
    Conflicts,
    /// Git failed after applying part of it, and kept the stash: `error`,
    /// and the untracked files it didn't restore because they exist.
    Partly { error: String, not_restored: Vec<String> },
}

/// Applies a stash (and drops it when `pop`).
pub fn unstash(root: &Path, index: usize, pop: bool) -> Result<Unstashed, String> {
    let name = format!("stash@{{{index}}}");
    let blockers = stash_blockers(root, index);
    let before = status(root)?.files;
    let Err(error) = run(root, &["stash", if pop { "pop" } else { "apply" }, &name]) else { return Ok(Unstashed::Applied) };
    let after = status(root)?.files;
    if after.iter().any(|f| f.staged == Some(Change::Conflicted)) {
        Ok(Unstashed::Conflicts)
    } else if after != before {
        Ok(Unstashed::Partly { error, not_restored: blockers })
    } else {
        Err(error)
    }
}

pub fn drop_stash(root: &Path, index: usize) -> Result<(), String> {
    run(root, &["stash", "drop", &format!("stash@{{{index}}}")]).map(drop)
}

// ---- History ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub time: i64,
    /// Local date and time, "2026-10-13 21:05".
    pub date: String,
    pub summary: String,
    /// Branch and tag names pointing here ("HEAD -> main", "origin/main", "tag: v1").
    pub refs: Vec<String>,
}

/// Up to `limit` commits reachable from HEAD (`all`: from every branch),
/// newest first; `path` limits it to one file's history, following renames.
pub fn log(root: &Path, limit: usize, all: bool, path: Option<&str>) -> Result<Vec<LogEntry>, String> {
    let mut extra = Vec::new();
    if all && path.is_none() {
        // Stashes are listed in the Commit view, not drawn as branches.
        extra.extend(["--exclude=refs/stash", "--all"]);
    }
    if let Some(path) = path {
        extra.extend(["--follow", "--", path]);
    }
    log_args(root, limit, &extra)
}

/// `git log` with our format and `extra` revisions or options.
fn log_args(root: &Path, limit: usize, extra: &[&str]) -> Result<Vec<LogEntry>, String> {
    let limit = format!("--max-count={limit}");
    let mut args = vec![
        "log",
        "--date-order",
        "--decorate=full",
        "--date=format-local:%Y-%m-%d %H:%M",
        &limit,
        "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ct%x1f%ad%x1f%D%x1f%s%x1e",
    ];
    args.extend_from_slice(extra);
    match run(root, &args) {
        Ok(out) => Ok(parse_log(&out)),
        // A repository without commits has no history.
        Err(e) if e.contains("does not have any commits") || e.contains("bad default revision") => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn parse_log(out: &str) -> Vec<LogEntry> {
    out.split('\x1e')
        .filter_map(|record| {
            let mut f = record.trim_start_matches('\n').split('\x1f');
            let sha = f.next()?.to_string();
            if sha.is_empty() {
                return None;
            }
            Some(LogEntry {
                sha,
                parents: f.next()?.split_whitespace().map(str::to_string).collect(),
                author: f.next()?.to_string(),
                email: f.next()?.to_string(),
                time: f.next()?.parse().unwrap_or(0),
                date: f.next()?.to_string(),
                refs: f
                    .next()?
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(|r| r.replace("refs/heads/", "").replace("refs/remotes/", "").replace("refs/tags/", ""))
                    .filter(|r| !r.ends_with("/HEAD"))
                    .collect(),
                summary: f.next()?.trim_end().to_string(),
            })
        })
        .collect()
}

/// A file changed by a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFile {
    pub change: Change,
    pub rel: String,
    /// The path before a rename.
    pub old_rel: Option<String>,
}

/// The full message and changed files of a commit (against its first parent).
pub fn commit_details(root: &Path, sha: &str) -> Result<(String, Vec<CommitFile>), String> {
    let message = run(root, &["log", "-1", "--format=%B", sha])?.trim_end().to_string();
    let out = run(root, &["show", "--first-parent", "-M", "--name-status", "-z", "--format=", sha])?;
    Ok((message, parse_name_status(&out)))
}

fn parse_name_status(out: &str) -> Vec<CommitFile> {
    let mut fields = out.split('\0').map(|f| f.trim_start_matches('\n')).filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let change = match status.chars().next() {
            Some('A') => Change::Added,
            Some('D') => Change::Deleted,
            Some('R') => Change::Renamed,
            Some('C') => Change::Added,
            _ => Change::Modified,
        };
        let (old_rel, rel) = if matches!(status.chars().next(), Some('R' | 'C')) {
            let old = fields.next().map(str::to_string);
            (old, fields.next())
        } else {
            (None, fields.next())
        };
        if let Some(rel) = rel {
            files.push(CommitFile { change, rel: rel.to_string(), old_rel });
        }
    }
    files
}

/// The raw bytes of a file version (`HEAD:path`, `:path` for the index),
/// `None` if it isn't there. For images and other binary files.
pub fn blob_bytes(root: &Path, spec: &str) -> Option<Vec<u8>> {
    let output = command(root, &["show", spec]).stderr(Stdio::null()).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// A file's contents at a revision (`sha:path`), `None` if it isn't there.
pub fn text_at(root: &Path, rev: &str, rel: &str) -> Option<String> {
    run(root, &["show", &format!("{rev}:{rel}")]).ok()
}

/// The texts a commit changed a file between: first parent → commit.
pub fn commit_file_texts(root: &Path, sha: &str, file: &CommitFile) -> (String, String) {
    let old_rel = file.old_rel.as_deref().unwrap_or(&file.rel);
    let base = if file.change == Change::Added { None } else { text_at(root, &format!("{sha}^"), old_rel) };
    let current = if file.change == Change::Deleted { None } else { text_at(root, sha, &file.rel) };
    (base.unwrap_or_default(), current.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_web_urls() {
        assert_eq!(web_url("git@github.com:owner/repo.git").as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(web_url("https://gitlab.com/group/sub/repo.git").as_deref(), Some("https://gitlab.com/group/sub/repo"));
        assert_eq!(web_url("https://user:tok@github.com/o/r").as_deref(), Some("https://github.com/o/r"));
        assert_eq!(web_url("ssh://git@host.dev:2222/o/r.git").as_deref(), Some("https://host.dev/o/r"));
        assert_eq!(web_url("/srv/git/repo.git"), None);
    }

    #[test]
    fn clone_folder_names() {
        assert_eq!(repo_name_from_url("https://github.com/o/kuana.git").as_deref(), Some("kuana"));
        assert_eq!(repo_name_from_url("git@github.com:o/kuana").as_deref(), Some("kuana"));
        assert_eq!(repo_name_from_url("https://github.com/o/kuana/").as_deref(), Some("kuana"));
        assert_eq!(repo_name_from_url("").as_deref(), None);
    }

    #[test]
    fn parses_porcelain_v2() {
        let root = Path::new("/repo");
        let out = "# branch.oid abc123\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/a.js\0\
1 A. N... 000000 100644 100644 000 ccc new.js\0\
2 R. N... 100644 100644 100644 ddd eee R100 renamed.js\0old.js\0\
u UU N... 100644 100644 100644 100644 f g h conflict.js\0\
? notes.txt\0\
! target/\0\
! logs/a.log\0";
        let s = parse_status(root, out);
        assert_eq!(s.ignored, [Path::new("/repo/target"), Path::new("/repo/logs/a.log")]);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!(s.files.len(), 5);
        let get = |rel: &str| s.files.iter().find(|f| f.rel == rel).unwrap().clone();
        assert_eq!(get("src/a.js").unstaged, Some(Change::Modified));
        assert_eq!(get("new.js").staged, Some(Change::Added));
        assert_eq!(get("renamed.js").staged, Some(Change::Renamed));
        assert_eq!(get("conflict.js").primary(), Change::Conflicted);
        assert_eq!(get("notes.txt").unstaged, Some(Change::Untracked));
        assert_eq!(s.staged().count(), 2);
        let map = s.path_map(root);
        assert_eq!(map.get(Path::new("/repo/src")), Some(&Change::Modified));
    }

    #[test]
    fn hunks() {
        let base = "a\nb\nc\nd\n";
        let h = diff_hunks(base, "a\nB\nc\nd\ne\n");
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].kind, h[0].new_rows.clone(), h[0].old_text.as_str()), (HunkKind::Modified, 1..2, "b\n"));
        assert_eq!((h[1].kind, h[1].new_rows.clone()), (HunkKind::Added, 4..5));
        let d = diff_hunks(base, "a\nd\n");
        assert_eq!((d[0].kind, d[0].new_rows.clone(), d[0].old_text.as_str()), (HunkKind::Deleted, 1..1, "b\nc\n"));
    }

    #[test]
    fn unified_diff_collapses_unchanged_runs() {
        let base: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let current = base.replace("line 10\n", "line ten\n");
        let lines = diff_document(&base, &current, 2).unified;
        assert_eq!(lines[0].kind, DiffLineKind::Skipped);
        assert_eq!(lines[0].text, "7 unchanged lines");
        let removed = lines.iter().find(|l| l.kind == DiffLineKind::Removed).unwrap();
        assert_eq!((removed.old_no, removed.text.as_str()), (Some(10), "line 10"));
        let added = lines.iter().find(|l| l.kind == DiffLineKind::Added).unwrap();
        assert_eq!((added.new_no, added.text.as_str()), (Some(10), "line ten"));
        assert_eq!(lines.last().unwrap().kind, DiffLineKind::Skipped);
    }

    #[test]
    fn error_lists_the_files_git_names() {
        let stderr = b"error: Your local changes to the following files would be overwritten by merge:\n\tapp.rs\n\tlib.rs\nMerge with strategy ort failed.\n";
        assert_eq!(error_text(stderr, b""), "Your local changes to the following files would be overwritten by merge: app.rs, lib.rs");
    }

    #[test]
    fn applies_selected_hunks() {
        let base = "a\nb\nc\nd\ne\n";
        let target = "a\nB\nc\nd\nE\nf\n";
        // Only the change touching row 1.
        assert_eq!(apply_hunks(base, target, |h| rows_touch(&h.new_rows, &(1..2))), "a\nB\nc\nd\ne\n");
        assert_eq!(apply_hunks(base, target, |h| rows_touch(&h.new_rows, &(4..6))), "a\nb\nc\nd\nE\nf\n");
        assert_eq!(apply_hunks(base, target, |_| true), target);
        // A deletion is picked by the row it sits at.
        assert_eq!(apply_hunks(base, "a\nd\ne\n", |h| rows_touch(&h.new_rows, &(1..1))), "a\nd\ne\n");
    }

    #[test]
    fn maps_rows_across_changes() {
        // Two lines inserted at the top shift everything after them.
        let base = "x\ny\nz\n";
        let target = "1\n2\nx\ny\nz\n";
        assert_eq!(map_rows_to_base(base, target, 3..4), 1..2);
        // A row inside a change snaps to the change in the base.
        assert_eq!(map_rows_to_base("a\nb\nc\n", "a\nB\nc\n", 1..2), 1..2);
    }

    #[test]
    fn word_level_emphasis() {
        assert_eq!(tokens("let x = foo(bar);"), ["let", " ", "x", " ", "=", " ", "foo", "(", "bar", ")", ";"]);
        let (old, new) = word_emphasis("let total = price * 2;", "let total = price * 3;");
        assert_eq!((old, new), (vec![20..21], vec![20..21]));
        // Unrelated lines get no emphasis: the whole line changed.
        assert_eq!(word_emphasis("fn main() {", "# heading").0, Vec::<Range<usize>>::new());
    }

    #[test]
    fn diffs_binary_and_cr_text_without_panicking() {
        // Bytes like an image's: lone \r and NUL bytes among others.
        let bytes = |seed: u32| (0..4000u32).map(|i| (i * seed % 256) as u8).collect::<Vec<u8>>();
        let png = String::from_utf8_lossy(&bytes(37)).into_owned();
        let other = String::from_utf8_lossy(&bytes(53)).into_owned();
        assert!(is_binary(&png) && !is_binary("plain\ntext\n"));
        diff_document(&png, &other, 3);
        diff_document("", &png, 3);
        diff_document("a\rb\rc\n", "a\rB\rc\n", 3);
        diff_hunks(&png, &other);
    }

    #[test]
    fn split_rows_pair_replacements() {
        let doc = diff_document("a\nb\nc\n", "a\nB\nB2\nc\n", 3);
        assert_eq!(doc.changes, [DiffChange { old_rows: 1..2, new_rows: 1..3 }]);
        assert_eq!((doc.added, doc.removed), (2, 1));
        let kinds: Vec<_> = doc.split.iter().map(|r| (r.left.as_ref().map(|s| s.no), r.right.as_ref().map(|s| s.no))).collect();
        assert_eq!(kinds, [(Some(1), Some(1)), (Some(2), Some(2)), (None, Some(3)), (Some(3), Some(4))]);
        assert_eq!(doc.split[1].change, Some((0, true)));
        assert_eq!(doc.unified.iter().filter(|l| l.change == Some((0, true))).count(), 1);
        // Everything, with nothing collapsed.
        let long: String = (0..50).map(|i| format!("{i}\n")).collect();
        let edited = long.replace("25\n", "x\n");
        assert!(diff_document(&long, &edited, 3).split.iter().any(|r| r.kind == DiffLineKind::Skipped));
        assert!(!diff_document(&long, &edited, usize::MAX).split.iter().any(|r| r.kind == DiffLineKind::Skipped));
    }

    #[test]
    fn parses_log_and_name_status() {
        let out = "abc\x1fp1 p2\x1fAda\x1fada@x.io\x1f1700000000\x1f2023-11-14 22:13\x1fHEAD -> refs/heads/main, refs/remotes/origin/main, refs/remotes/origin/HEAD, tag: refs/tags/v1\x1fMerge it\n\x1e\ndef\x1f\x1fBob\x1fb@x.io\x1f1600000000\x1f2020-09-13 12:26\x1f\x1fFirst\n\x1e\n";
        let log = parse_log(out);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].parents, ["p1", "p2"]);
        assert_eq!(log[0].refs, ["HEAD -> main", "origin/main", "tag: v1"]);
        assert_eq!(log[0].date, "2023-11-14 22:13");
        assert_eq!((log[1].summary.as_str(), log[1].parents.len(), log[1].refs.len()), ("First", 0, 0));
        let files = parse_name_status("M\0src/a.rs\0R087\0old.rs\0new.rs\0A\0b.txt\0");
        assert_eq!(files[1], CommitFile { change: Change::Renamed, rel: "new.rs".into(), old_rel: Some("old.rs".into()) });
        assert_eq!((files[0].change, files[2].change), (Change::Modified, Change::Added));
    }

    #[test]
    fn git_runs_on_the_given_folder_whatever_the_environment_says() {
        let cmd = command(Path::new("/some/project"), &["status"]);
        let removed: Vec<_> = cmd.get_envs().filter(|(_, value)| value.is_none()).map(|(key, _)| key.to_string_lossy().into_owned()).collect();
        for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"] {
            assert!(removed.iter().any(|r| r == var), "{var} isn't cleared: {removed:?}");
        }
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/some/project")));
    }

    /// In a folder that isn't trusted, what the repository's config names
    /// to run is turned off, and diffs use no external program.
    #[test]
    fn reads_cat_file_batches() {
        let out = b"abc blob 5\nhello\nHEAD:gone.rs missing\ndef blob 0\n\n";
        assert_eq!(parse_batch(out, 3).unwrap(), [Some(b"hello".to_vec()), None, Some(Vec::new())]);
        assert!(parse_batch(b"abc blob 5\nhel", 1).unwrap() == [None] || parse_batch(b"abc blob 5\nhel", 1).is_err());
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        for (k, v) in [("user.name", "Test"), ("user.email", "t@example.com")] {
            run(&dir, &["config", k, v]).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&dir, &["add", "a.txt"]).unwrap();
        run(&dir, &["commit", "-m", "first"]).unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        run(&dir, &["add", "a.txt"]).unwrap();
        let specs = ["HEAD:a.txt", ":a.txt", "HEAD:none.txt"].map(String::from);
        let blobs = read_blobs(&dir, &specs).unwrap();
        assert_eq!(blobs, [Some(b"one\n".to_vec()), Some(b"two\n".to_vec()), None]);
        // The status keeps each file's HEAD and index blobs.
        let status = status(&dir).unwrap();
        assert!(status.files[0].blobs.as_deref().is_some_and(|b| b.split(' ').count() == 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untrusted_repositories_run_none_of_their_programs() {
        let args = |cmd: &Command| cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let untrusted = Path::new("/kuana-untrusted-test/repo");
        let diff = args(&command_in(untrusted, &["diff", "--", "a.rs"], false));
        for setting in UNTRUSTED_CONFIG.iter().chain(&["core.fsmonitor=false", "log.showSignature=false"]) {
            assert!(diff.iter().any(|a| a == setting), "{setting} missing: {diff:?}");
        }
        let at = |name: &str| diff.iter().position(|a| a == name).unwrap();
        assert_eq!(at("--no-ext-diff"), at("diff") + 1, "{diff:?}");
        assert!(at("--no-ext-diff") < at("--"));
        assert!(!args(&command_in(untrusted, &["status"], false)).iter().any(|a| a == "--no-ext-diff"));
        assert!(!args(&command_in(untrusted, &["diff"], true)).iter().any(|a| a == "--no-ext-diff" || a == "credential.helper="));
    }

    #[test]
    fn git_runs_with_the_path_language_servers_get() {
        // Hooks, git-lfs and credential helpers find what a terminal finds,
        // also when Kuana was started from the Dock.
        let cmd = command_in(Path::new("/kuana-path-test"), &["status"], true);
        let path = cmd.get_envs().find(|(name, _)| *name == "PATH").and_then(|(_, value)| value);
        assert_eq!(path, Some(crate::lsp::servers::search_path().as_os_str()));
    }

    #[test]
    fn staging_lines_keeps_crlf_and_the_encoding() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-crlf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        for (k, v) in [("user.name", "Test"), ("user.email", "t@example.com"), ("core.autocrlf", "false")] {
            run(&dir, &["config", k, v]).unwrap();
        }
        let root = discover(&dir).unwrap();
        // CRLF line ends and a Windows-1252 "é" (0xE9) on every line.
        let file = |two: &str, five: &str| -> Vec<u8> {
            (1..=6)
                .flat_map(|i| {
                    let mut line = match i {
                        2 => two.to_string(),
                        5 => five.to_string(),
                        _ => format!("line {i}"),
                    }
                    .into_bytes();
                    line.extend([b' ', 0xE9, b'\r', b'\n']);
                    line
                })
                .collect()
        };
        let lines = file("line 2", "line 5");
        std::fs::write(dir.join("w.txt"), &lines).unwrap();
        stage(&root, &["w.txt".into()]).unwrap();
        commit(&root, "first", false).unwrap();

        // The editor's view: decoded, LF; it matches HEAD where unchanged.
        let head = BlobText::new(&head_blob(&root, "w.txt").unwrap(), None);
        assert_eq!(head.text, (1..=6).map(|i| format!("line {i} \u{e9}\n")).collect::<String>());
        // Two changes; staging the second changes only its line in the index.
        let edited = head.text.replace("line 2 ", "line two ").replace("line 5 ", "line five ");
        stage_lines(&root, "w.txt", &edited, 4..5).unwrap();
        assert_eq!(index_blob(&root, "w.txt").unwrap(), file("line 2", "line five"));
        unstage_lines(&root, "w.txt", Some(&edited), 4..5).unwrap();
        assert_eq!(index_blob(&root, "w.txt").unwrap(), lines);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_trashes_untracked_files() {
        if !is_available() {
            return;
        }
        let root = test_repo("discard-trash");
        std::fs::write(root.join("kept.txt"), "one\n").unwrap();
        stage(&root, &["kept.txt".into()]).unwrap();
        commit(&root, "first", false).unwrap();
        std::fs::write(root.join("kept.txt"), "two\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let trashed = std::cell::RefCell::new(Vec::new());
        discard_with(&root, &status(&root).unwrap().files, |path| {
            trashed.borrow_mut().push(path.to_path_buf());
            std::fs::remove_file(path)
        })
        .unwrap();
        assert_eq!(trashed.into_inner(), [root.join("new.txt")]);
        assert_eq!(std::fs::read_to_string(root.join("kept.txt")).unwrap(), "one\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_not_patterns() {
        if !is_available() {
            return;
        }
        // As a pattern `[A].sh` is `A.sh`, which sorts before it.
        let root = test_repo("literal");
        for rel in ["[A].sh", "A.sh"] {
            std::fs::write(root.join(rel), "one\n").unwrap();
        }
        stage(&root, &["[A].sh".into(), "A.sh".into()]).unwrap();
        run(&root, &["update-index", "--chmod=+x", "--", "A.sh"]).unwrap();
        commit(&root, "first", false).unwrap();
        for rel in ["[A].sh", "A.sh"] {
            std::fs::write(root.join(rel), "two\n").unwrap();
        }
        let staged = || -> Vec<String> {
            status(&root).unwrap().files.into_iter().filter(|f| f.staged.is_some()).map(|f| f.rel).collect()
        };
        stage(&root, &["[A].sh".into()]).unwrap();
        assert_eq!(staged(), ["[A].sh"]);
        unstage(&root, &["[A].sh".into()], true).unwrap();
        assert!(staged().is_empty());
        // Partial staging keeps the file's own mode, not `A.sh`'s.
        stage_lines(&root, "[A].sh", "two\n", 0..1).unwrap();
        let mode = run(&root, &["--literal-pathspecs", "ls-files", "--stage", "--", "[A].sh"]).unwrap();
        assert!(mode.starts_with("100644"), "{mode}");
        unstage(&root, &["[A].sh".into()], true).unwrap();
        let files = status(&root).unwrap().files;
        discard_here(&root, &files.into_iter().filter(|f| f.rel == "[A].sh").collect::<Vec<_>>()).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("[A].sh")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(root.join("A.sh")).unwrap(), "two\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_lists_what_git_ignores() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        test_init(&dir);
        // Not the machine's own global excludes.
        let excludes = std::env::temp_dir().join(format!("kuana-git-excludes-{}", std::process::id()));
        std::fs::write(&excludes, "").unwrap();
        run(&dir, &["config", "core.excludesFile", &excludes.to_string_lossy()]).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(dir.join("sub/.gitignore"), "secret.txt\n").unwrap();
        for file in ["target/debug/a.o", "target/b.o", "run.log", "sub/secret.txt", "sub/keep.rs"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let root = discover(&dir).unwrap();
        let mut ignored = status(&root).unwrap().ignored;
        ignored.sort();
        // The folder once, not what's in it; nested .gitignore files count.
        assert_eq!(ignored, ["run.log", "sub/secret.txt", "target"].map(|rel| root.join(rel)));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&excludes).ok();
    }

    #[test]
    fn partial_staging_merges_and_history() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-hunks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        for (k, v) in [("user.name", "Test"), ("user.email", "t@example.com")] {
            run(&dir, &["config", k, v]).unwrap();
        }
        // `main` whatever the machine's init.defaultBranch (Git's own default is master).
        run(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        let root = discover(&dir).unwrap();
        let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("f.txt"), &lines).unwrap();
        stage(&root, &["f.txt".into()]).unwrap();
        commit(&root, "first", false).unwrap();

        // Two separate changes; stage only the second.
        let edited = lines.replace("line 2\n", "line two\n").replace("line 11\n", "line eleven\n");
        std::fs::write(dir.join("f.txt"), &edited).unwrap();
        stage_lines(&root, "f.txt", &edited, 10..11).unwrap();
        let index = index_text(&root, "f.txt").unwrap();
        assert!(index.contains("line eleven") && index.contains("line 2\n"));
        let s = status(&root).unwrap();
        assert_eq!((s.files[0].staged, s.files[0].unstaged), (Some(Change::Modified), Some(Change::Modified)));
        // Unstage it again, addressing it by the editor's row.
        unstage_lines(&root, "f.txt", Some(&edited), 10..11).unwrap();
        assert_eq!(index_text(&root, "f.txt").unwrap(), lines);

        // History and commit details.
        stage(&root, &["f.txt".into()]).unwrap();
        commit(&root, "edit", false).unwrap();
        let history = log(&root, 10, false, Some("f.txt")).unwrap();
        assert_eq!(history.iter().map(|e| e.summary.as_str()).collect::<Vec<_>>(), ["edit", "first"]);
        let (message, files) = commit_details(&root, &history[0].sha).unwrap();
        assert_eq!((message.as_str(), files[0].rel.as_str()), ("edit", "f.txt"));
        let (old, new) = commit_file_texts(&root, &history[0].sha, &files[0]);
        assert_eq!((old, new), (lines.clone(), edited.clone()));

        // A conflicting merge stops in the Merging state; abort restores.
        create_branch(&root, "other").unwrap();
        std::fs::write(dir.join("f.txt"), edited.replace("line 6\n", "theirs\n")).unwrap();
        stage(&root, &["f.txt".into()]).unwrap();
        commit(&root, "theirs", false).unwrap();
        run(&root, &["switch", "main"]).unwrap();
        std::fs::write(dir.join("f.txt"), edited.replace("line 6\n", "ours\n")).unwrap();
        stage(&root, &["f.txt".into()]).unwrap();
        commit(&root, "ours", false).unwrap();
        assert_eq!(merge(&root, "other").unwrap(), "CONFLICT");
        assert_eq!(repo_state(&root), Some(RepoState::Merging));
        assert!(merge_message(&root).unwrap().starts_with("Merge branch 'other'"));
        let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert!(text.contains("<<<<<<<") && text.contains("theirs"));
        abort(&root, RepoState::Merging).unwrap();
        assert_eq!(repo_state(&root), None);
        // Resolve and continue this time.
        merge(&root, "other").unwrap();
        std::fs::write(dir.join("f.txt"), edited.replace("line 6\n", "both\n")).unwrap();
        stage(&root, &["f.txt".into()]).unwrap();
        continue_op(&root, RepoState::Merging).unwrap();
        assert_eq!(repo_state(&root), None);
        assert_eq!(log(&root, 1, false, None).unwrap()[0].parents.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remotes_branches_stashes_and_commit_actions() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-actions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        run(&dir, &["config", "user.name", "Test"]).unwrap();
        run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        let root = discover(&dir).unwrap();
        let file = dir.join("a.txt");
        let commit_text = |text: &str, message: &str| {
            std::fs::write(&file, text).unwrap();
            stage(&root, &["a.txt".into()]).unwrap();
            commit(&root, message, false).unwrap();
            run(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string()
        };
        let first = commit_text("one\n", "first");

        // Remotes.
        assert_eq!(default_remote(&root), None);
        add_remote(&root, "upstream", "https://example.com/u.git").unwrap();
        add_remote(&root, "origin", "https://example.com/o.git").unwrap();
        assert_eq!(default_remote(&root).as_deref(), Some("origin"));
        set_remote_url(&root, "origin", "git@example.com:o.git").unwrap();
        rename_remote(&root, "upstream", "fork").unwrap();
        let names: Vec<(String, String)> = remotes(&root).unwrap().into_iter().map(|r| (r.name, r.url)).collect();
        assert_eq!(names, vec![("fork".into(), "https://example.com/u.git".into()), ("origin".into(), "git@example.com:o.git".into())]);
        remove_remote(&root, "fork").unwrap();
        assert_eq!(remotes(&root).unwrap().len(), 1);

        // Branches: a merged one deletes plainly, an unmerged one needs force.
        let main = status(&root).unwrap().branch.unwrap();
        assert!(valid_branch_name(&root, "feature/x") && !valid_branch_name(&root, "bad..name"));
        create_branch_at(&root, "side", &first).unwrap();
        let side = commit_text("side\n", "side change");
        checkout(&root, &Branch { name: main.clone(), remote: false, current: false }).unwrap();
        assert!(!is_merged(&root, "side"));
        assert!(delete_branch(&root, "side", false).is_err());
        rename_branch(&root, "side", "side2").unwrap();
        delete_branch(&root, "side2", true).unwrap();

        // Commits: tag, cherry-pick (the deleted branch's commit still exists), revert, reset.
        create_tag(&root, "v1", &first).unwrap();
        assert!(run(&root, &["tag"]).unwrap().contains("v1"));
        assert_ne!(cherry_pick(&root, &side).unwrap(), "CONFLICT");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "side\n");
        revert(&root, "HEAD").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        reset(&root, &first, ResetMode::Soft).unwrap();
        assert!(status(&root).unwrap().files.is_empty(), "the revert undid the cherry-pick");
        let second = commit_text("two\n", "second");
        reset(&root, &first, ResetMode::Mixed).unwrap();
        assert_eq!(status(&root).unwrap().files[0].unstaged, Some(Change::Modified));
        reset(&root, &first, ResetMode::Hard).unwrap();
        assert!(status(&root).unwrap().files.is_empty());
        checkout_commit(&root, &second).unwrap();
        assert_eq!(status(&root).unwrap().branch, None);
        checkout(&root, &Branch { name: main.clone(), remote: false, current: false }).unwrap();

        // Stashes take untracked files only when asked to.
        std::fs::write(&file, "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        assert!(stash(&root, Some("work in progress"), false).unwrap());
        let left: Vec<String> = status(&root).unwrap().files.into_iter().map(|f| f.rel).collect();
        assert_eq!(left, ["new.txt"]);
        let list = stashes(&root);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].index, list[0].message.ends_with("work in progress")), (0, true));
        // Only an untracked file left: nothing to stash without it.
        assert!(!stash(&root, None, false).unwrap());
        assert_eq!(stashes(&root).len(), 1);
        assert_eq!(unstash(&root, 0, false).unwrap(), Unstashed::Applied);
        assert_eq!(stashes(&root).len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed\n");
        discard_here(&root, &status(&root).unwrap().unstaged().cloned().collect::<Vec<_>>()).unwrap();
        assert_eq!(unstash(&root, 0, true).unwrap(), Unstashed::Applied);
        assert!(stashes(&root).is_empty());
        assert!(stash(&root, None, true).unwrap());
        assert!(status(&root).unwrap().files.is_empty());
        drop_stash(&root, 0).unwrap();
        assert!(stashes(&root).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The status of the paths that changed, merged into the last full one,
    /// is what a full status says now.
    #[test]
    fn a_status_of_changed_paths_merges_into_the_last_full_one() {
        if !is_available() {
            return;
        }
        let root = test_repo("scoped");
        let write = |rel: &str, text: &str| {
            std::fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
            std::fs::write(root.join(rel), text).unwrap();
        };
        write(".gitignore", "dist/\n*.log\n");
        for rel in ["src/a.rs", "src/b.rs", "keep.txt", "old.log"] {
            write(rel, "one\n");
        }
        stage(&root, &[".gitignore".into(), "src".into(), "keep.txt".into()]).unwrap();
        commit(&root, "first", false).unwrap();
        run(&root, &["mv", "src/b.rs", "src/c.rs"]).unwrap();
        write("src/a.rs", "staged\n");
        stage(&root, &["src/a.rs".into()]).unwrap();
        let mut known = status(&root).unwrap();
        let check = |known: &RepoStatus| {
            let full = status(&root).unwrap();
            assert_eq!(known.files, full.files);
            let sorted = |mut paths: Vec<PathBuf>| {
                paths.sort();
                paths
            };
            assert_eq!(sorted(known.ignored.clone()), sorted(full.ignored));
        };

        // The work tree changes: the renamed file and the staged one edited,
        // a new file, a removed one, build output in an ignored folder, a log.
        write("src/c.rs", "changed\n");
        write("src/a.rs", "staged, then edited\n");
        write("src/new.rs", "new\n");
        std::fs::remove_file(root.join("keep.txt")).unwrap();
        write("dist/x/out.js", "built\n");
        write("debug.log", "log\n");
        let rels: Vec<String> = ["src/c.rs", "src/a.rs", "src/new.rs", "keep.txt", "dist/x/out.js", "debug.log"].map(String::from).into();
        known.merge(&root, &rels, status_of(&root, &rels).unwrap());
        check(&known);
        assert_eq!(known.files.iter().find(|f| f.rel == "src/c.rs").and_then(|f| f.staged), Some(Change::Renamed));
        // A new file where the renamed one was: alone, its path also reads
        // as the rename's deletion.
        write("src/b.rs", "again\n");
        let rels = vec!["src/b.rs".to_string()];
        known.merge(&root, &rels, status_of(&root, &rels).unwrap());
        check(&known);

        // A folder at once: what went from it goes, what's ignored stays.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::fs::remove_dir_all(root.join("dist")).unwrap();
        std::fs::remove_file(root.join("old.log")).unwrap();
        let rels = vec!["src".to_string(), "dist".to_string(), "old.log".to_string()];
        known.merge(&root, &rels, status_of(&root, &rels).unwrap());
        check(&known);
        assert!(status_of(&root, &[]).unwrap().files.is_empty(), "no paths isn't every path");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pathspecs_cover_each_path_once() {
        let root = std::env::temp_dir().join(format!("kuana-git-specs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/.git")).unwrap();
        let specs = |paths: &[PathBuf]| pathspecs(&root, paths.iter().map(PathBuf::as_path));
        // A folder covers what's in it; a nested repository reads as its
        // folder; what's outside the repository is left out.
        let paths = ["src/a.rs", "src", "sub/x/y.txt", "notes.txt"].map(|p| root.join(p));
        let outside = std::env::temp_dir().join("elsewhere.txt");
        assert_eq!(specs(&[&paths[..], &[outside]].concat()), Some(["notes.txt", "src", "sub"].map(String::from).to_vec()));
        // Too many: the deepest go to their folders.
        let many: Vec<PathBuf> = (0..200).map(|i| root.join(format!("gen/d{}/f{i}.rs", i % 4))).collect();
        assert_eq!(specs(&many), Some(["gen/d0", "gen/d1", "gen/d2", "gen/d3"].map(String::from).to_vec()));
        // Up to the repository itself: read all of it.
        let top: Vec<PathBuf> = (0..100).map(|i| root.join(format!("f{i}.rs"))).collect();
        assert_eq!(specs(&top), None);
        assert_eq!(specs(&[root.join(".gitignore").parent().unwrap().to_path_buf()]), None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// `discard`, deleting untracked files rather than filling the Trash.
    fn discard_here(root: &Path, files: &[FileStatus]) -> Result<(), String> {
        discard_with(root, files, |path| std::fs::remove_file(path))
    }

    /// `git init`, with the files Git writes kept byte for byte: Git for
    /// Windows' `core.autocrlf=true` (the CI runners' default) would check
    /// out `\r\n` where the tests wrote `\n`.
    fn test_init(dir: &Path) {
        init(dir).unwrap();
        run(dir, &["config", "core.autocrlf", "false"]).unwrap();
    }

    /// A repository in a fresh temp folder, with a committer and `main` checked out.
    fn test_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        run(&dir, &["config", "user.name", "Test"]).unwrap();
        run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        run(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        discover(&dir).unwrap()
    }

    #[test]
    fn commits_and_stashes_leave_untracked_files_alone() {
        if !is_available() {
            return;
        }
        let root = test_repo("untracked");
        let write = |rel: &str, text: &str| std::fs::write(root.join(rel), text).unwrap();
        write("a.txt", "one\n");
        write("gone.txt", "bye\n");
        stage(&root, &["a.txt".into(), "gone.txt".into()]).unwrap();
        commit(&root, "first", false).unwrap();

        // Commit All: changes to tracked files (a deletion too), not new files.
        write("a.txt", "two\n");
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        write("Cargo.lock", "generated\n");
        let s = status(&root).unwrap();
        let rels = |files: Vec<&FileStatus>| files.into_iter().map(|f| f.rel.clone()).collect::<Vec<_>>();
        assert_eq!(rels(s.unstaged().collect()), ["a.txt", "gone.txt"]);
        assert_eq!(rels(s.untracked().collect()), ["Cargo.lock"]);
        stage_tracked(&root).unwrap();
        commit(&root, "second", false).unwrap();
        let s = status(&root).unwrap();
        assert_eq!(rels(s.files.iter().collect()), ["Cargo.lock"]);
        assert_eq!(s.files[0].unstaged, Some(Change::Untracked));

        // A stash with untracked files, popped after one of them came back:
        // Git applies the rest and keeps the stash, which is reported.
        write("a.txt", "three\n");
        write("TODO.txt", "todo\n");
        assert!(stash(&root, Some("wip"), true).unwrap());
        assert!(status(&root).unwrap().files.is_empty());
        assert!(stash_blockers(&root, 0).is_empty());
        write("Cargo.lock", "regenerated\n");
        assert_eq!(stash_blockers(&root, 0), ["Cargo.lock"]);
        match unstash(&root, 0, true).unwrap() {
            Unstashed::Partly { not_restored, .. } => assert_eq!(not_restored, ["Cargo.lock"]),
            other => panic!("expected a partly applied stash, got {other:?}"),
        }
        assert_eq!(stashes(&root).len(), 1, "the stash is kept");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "three\n");
        assert!(root.join("TODO.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("Cargo.lock")).unwrap(), "regenerated\n");
        // Nothing applies a second time: Git refuses and nothing changes.
        assert!(unstash(&root, 0, true).is_err());
        assert_eq!(stashes(&root).len(), 1);

        // With the files out of the way, the whole stash applies.
        discard_here(&root, &status(&root).unwrap().files).unwrap();
        assert!(stash_blockers(&root, 0).is_empty());
        assert_eq!(unstash(&root, 0, true).unwrap(), Unstashed::Applied);
        assert_eq!(std::fs::read_to_string(root.join("Cargo.lock")).unwrap(), "generated\n");
        assert!(stashes(&root).is_empty());

        // A stashed untracked folder that's a file now isn't a blocker Git
        // names, but it stops the restore after the tracked changes applied.
        std::fs::create_dir_all(root.join("gen")).unwrap();
        write("gen/out.txt", "x\n");
        assert!(stash(&root, None, true).unwrap());
        write("gen", "a file now\n");
        assert!(stash_blockers(&root, 0).is_empty());
        match unstash(&root, 0, true).unwrap() {
            Unstashed::Partly { not_restored, error } => assert!(not_restored.is_empty() && !error.is_empty(), "{error}"),
            other => panic!("expected a partly applied stash, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "three\n");
        assert_eq!(stashes(&root).len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pull_strategies() {
        if !is_available() {
            return;
        }
        // Ours, a bare remote, and a second clone standing in for someone else.
        let ours = test_repo("pull");
        let remote = ours.with_extension("remote.git");
        let theirs = ours.with_extension("theirs");
        for dir in [&remote, &theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
        run(&ours, &["init", "--bare", "--quiet", remote.to_str().unwrap()]).unwrap();
        run(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        run(&ours, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        let commit_in = |root: &Path, rel: &str, text: &str| {
            std::fs::write(root.join(rel), text).unwrap();
            stage(root, &[rel.to_string()]).unwrap();
            commit(root, &format!("{rel}: {}", text.trim()), false).unwrap();
        };
        commit_in(&ours, "a.txt", "one\n");
        push(&ours, Some(("origin", "main")), false).unwrap();
        run(&ours, &["clone", "--quiet", "--config", "core.autocrlf=false", remote.to_str().unwrap(), theirs.to_str().unwrap()]).unwrap();
        run(&theirs, &["config", "user.name", "Them"]).unwrap();
        run(&theirs, &["config", "user.email", "them@example.com"]).unwrap();
        let publish = |rel: &str, text: &str| {
            run(&theirs, &["pull", "--quiet", "--ff-only"]).unwrap();
            commit_in(&theirs, rel, text);
            push(&theirs, None, false).unwrap();
        };
        let head = || run(&ours, &["rev-parse", "HEAD"]).unwrap();

        assert_eq!(pull(&ours, None).unwrap(), Pulled::UpToDate);
        publish("b.txt", "theirs\n");
        assert_eq!(pull(&ours, None).unwrap(), Pulled::FastForwarded(1));

        // Both sides have new commits: a fast-forward-only pull changes nothing.
        publish("c.txt", "theirs\n");
        commit_in(&ours, "d.txt", "ours\n");
        let before = head();
        let diverged = Pulled::Diverged { branch: "main".into(), upstream: "origin/main".into(), ahead: 1, behind: 1 };
        assert_eq!(pull(&ours, Some(PullStrategy::FfOnly)).unwrap(), diverged);
        // Without a strategy, unless this machine's Git config has one.
        if !pull_configured(&ours, Some("main")) {
            assert_eq!(pull(&ours, None).unwrap(), diverged);
        }
        assert_eq!(head(), before);
        assert_eq!(pull(&ours, Some(PullStrategy::Merge)).unwrap(), Pulled::Merged(1));
        assert_eq!(log(&ours, 1, false, None).unwrap()[0].parents.len(), 2);
        push(&ours, None, false).unwrap();

        // Rebasing keeps the history straight: ours on top of theirs.
        publish("e.txt", "theirs\n");
        commit_in(&ours, "f.txt", "ours\n");
        assert_eq!(pull(&ours, Some(PullStrategy::Rebase)).unwrap(), Pulled::Rebased(1));
        let history = log(&ours, 2, false, None).unwrap();
        assert_eq!((history[0].summary.as_str(), history[1].summary.as_str()), ("f.txt: ours", "e.txt: theirs"));
        assert_eq!(history[0].parents.len(), 1);
        assert_eq!(pull(&ours, Some(PullStrategy::Rebase)).unwrap(), Pulled::UpToDate);

        // Conflicting changes stop the rebase, to continue or abort.
        publish("a.txt", "theirs\n");
        commit_in(&ours, "a.txt", "ours\n");
        assert_eq!(pull(&ours, Some(PullStrategy::Rebase)).unwrap(), Pulled::Conflicts);
        assert_eq!(repo_state(&ours), Some(RepoState::Rebasing));
        abort(&ours, RepoState::Rebasing).unwrap();
        assert_eq!(std::fs::read_to_string(ours.join("a.txt")).unwrap(), "ours\n");
        for dir in [&ours, &remote, &theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn real_repository_round_trip() {
        if !is_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kuana-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        test_init(&dir);
        run(&dir, &["config", "user.name", "Test"]).unwrap();
        run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let root = discover(&dir).unwrap();
        let s = status(&root).unwrap();
        assert_eq!(s.files[0].unstaged, Some(Change::Untracked));
        stage(&root, &["a.txt".into()]).unwrap();
        assert_eq!(status(&root).unwrap().staged().count(), 1);
        commit(&root, "first", false).unwrap();
        assert_eq!(head_text(&root, "a.txt").as_deref(), Some("one\ntwo\n"));
        std::fs::write(dir.join("a.txt"), "one\n2\n").unwrap();
        let s = status(&root).unwrap();
        assert_eq!(s.files[0].unstaged, Some(Change::Modified));
        let blame = blame_file(&root, "a.txt", b"one\n2\nthree\n", &KillOnDrop::default().token());
        let blame = blame.unwrap_or_else(|_| panic!("blamed"));
        assert_eq!(blame.line(0).map(|l| (l.summary.as_str(), l.author.as_str(), l.uncommitted)), Some(("first", "Test", false)));
        assert!(blame.line(1).is_some_and(|l| l.uncommitted) && blame.line(2).is_some_and(|l| l.uncommitted));
        assert_eq!(blame.line(3), None);
        // A blame whose owner went is stopped, not run.
        let owner = KillOnDrop::default();
        let token = owner.token();
        drop(owner);
        assert!(blame_file(&root, "a.txt", b"one\n", &token).is_err());
        discard_here(&root, &s.files).unwrap();
        assert!(status(&root).unwrap().files.is_empty());
        create_branch(&root, "feature").unwrap();
        assert!(branches(&root).unwrap().iter().any(|b| b.name == "feature" && b.current));

        // Push to a bare "remote", then fetch and pull from it.
        let remote = dir.with_extension("remote.git");
        let _ = std::fs::remove_dir_all(&remote);
        run(&dir, &["init", "--bare", "--quiet", remote.to_str().unwrap()]).unwrap();
        assert!(default_remote(&root).is_none());
        run(&root, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        assert!(default_remote(&root).is_some());
        // Before pushing, the preview lists the unpushed commit for a new branch.
        let preview = push_preview(&root).unwrap();
        assert_eq!((preview.branch.as_str(), preview.upstream.as_deref(), preview.commits.len()), ("feature", None, 1));
        push(&root, Some(("origin", "feature")), false).unwrap();
        let preview = push_preview(&root).unwrap();
        assert_eq!((preview.upstream.as_deref(), preview.commits.len()), (Some("origin/feature"), 0));
        let s = status(&root).unwrap();
        assert_eq!(s.upstream.as_deref(), Some("origin/feature"));
        assert_eq!((s.ahead, s.behind), (0, 0));
        fetch(&root).unwrap();
        assert_eq!(pull(&root, None).unwrap(), Pulled::UpToDate);
        assert!(branches(&root).unwrap().iter().any(|b| b.remote && b.name == "origin/feature"));
        assert_eq!(remote_url(&root).as_deref(), remote.to_str());
        assert_eq!(last_commit(&root).unwrap().summary, "first");

        // Clone the remote back, shallow, with progress.
        let parent = dir.with_extension("clones");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        run(&remote, &["symbolic-ref", "HEAD", "refs/heads/feature"]).unwrap();
        let url = url::Url::from_file_path(&remote).unwrap().to_string();
        let job = start_clone(&parent, &url, "copy", true).unwrap();
        let cloned = job.wait().unwrap();
        // Windows runners check out with core.autocrlf.
        assert_eq!(std::fs::read_to_string(cloned.join("a.txt")).unwrap().replace("\r\n", "\n"), "one\ntwo\n");
        // A failing clone reports git's error.
        let err = start_clone(&parent, "file:///nonexistent/kuana.git", "nope", false).unwrap().wait().unwrap_err();
        assert!(!err.is_empty() && !parent.join("nope").exists(), "{err}");
        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }
}
