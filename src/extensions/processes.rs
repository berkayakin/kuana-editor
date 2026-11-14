//! Programs extensions' code starts and leaves running (`spawn-process` of
//! interface 0.2): a log being followed, a build, a watcher. Their output is
//! read on threads of their own and handed over in batches, at most one every
//! `BATCH_EVERY`, so a program that prints fast costs the UI a few updates a
//! second, not one per line. A program ends when its extension asks
//! (`kill-process`), when the extension goes, when its window closes and when
//! Kuana quits. One its extension ends is gone at once: its name is free for
//! the next, and nothing more of it is told (`Ended`), so the code never takes
//! the old program's last lines or its end for the new one's.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// How many programs one extension may have running.
pub const MAX_RUNNING: usize = 4;
/// How often output is handed over while it keeps coming.
const BATCH_EVERY: Duration = Duration::from_millis(100);
/// The most lines in one batch (more wait for the next).
const BATCH_LINES: usize = 500;
/// A longer line is cut: a panel shows lines, not megabytes.
const MAX_LINE: usize = 4000;
/// Lines read but not handed over yet; past it the program waits to write.
const WAITING_LINES: usize = 2000;

/// What a running program's owner hears.
#[derive(Debug, PartialEq)]
pub enum Told {
    /// Lines it printed (both its outputs, as they came).
    Lines(Vec<String>),
    /// It ended, with this exit code (-1: by a signal, or ended by Kuana).
    Exited(i32),
}

struct Running {
    id: u64,
    window: u64,
    child: Arc<Mutex<Child>>,
    ended: Ended,
}

/// Set when the extension ended the program with `kill`: what it printed or
/// how it ended is dropped from then on, also where it waits to be handed to
/// the code (the requests and events carry a copy and check it).
#[derive(Clone, Default)]
pub struct Ended(Arc<AtomicBool>);

impl std::fmt::Debug for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ended({})", self.is_set())
    }
}

/// The same flag (events compare equal in tests by what they say).
impl PartialEq for Ended {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Ended {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The programs running, by (extension, name).
static RUNNING: LazyLock<Mutex<HashMap<(String, String), Running>>> = LazyLock::new(Default::default);

fn running() -> std::sync::MutexGuard<'static, HashMap<(String, String), Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts `command` as `extension`'s program `name`, for `window`. `tell`
/// gets its output and its end, on a thread of the program's own, until
/// `ended` is set.
pub fn start(extension: &str, name: &str, window: u64, mut command: Command, ended: Ended, tell: impl Fn(Told) + Send + 'static) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let key = (extension.to_string(), name.to_string());
    let mut all = running();
    if all.contains_key(&key) {
        return Err(format!("{name} is already running"));
    }
    if all.keys().filter(|(owner, _)| owner == extension).count() >= MAX_RUNNING {
        return Err(format!("{MAX_RUNNING} programs are running already"));
    }
    // Its own process group, so ending it reaches what it started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("couldn't run it: {e}"))?;
    let (lines, read) = mpsc::sync_channel::<String>(WAITING_LINES);
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)] {
        let (Some(pipe), lines) = (pipe, lines.clone()) else { continue };
        std::thread::spawn(move || read_lines(pipe, lines));
    }
    drop(lines);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let child = Arc::new(Mutex::new(child));
    all.insert(key.clone(), Running { id, window, child: child.clone(), ended: ended.clone() });
    drop(all);
    let tell = move |told: Told| {
        if !ended.is_set() {
            tell(told)
        }
    };
    std::thread::Builder::new()
        .name(format!("extension process {extension}/{name}"))
        .spawn(move || {
            hand_over(read, &tell);
            // Its outputs are closed; it ends in a moment, if it hasn't.
            let status = loop {
                match child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() {
                    Ok(Some(status)) => break status.code().unwrap_or(-1),
                    Ok(None) => {}
                    Err(_) => break -1,
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let mut all = running();
            if all.get(&key).is_some_and(|r| r.id == id) {
                all.remove(&key);
            }
            drop(all);
            tell(Told::Exited(status));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reads a program's output line by line (not as UTF-8: a program may print
/// anything), until it closes or nobody listens.
fn read_lines(pipe: impl Read, lines: mpsc::SyncSender<String>) {
    let mut reader = BufReader::new(pipe);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        while line.last().is_some_and(|b| matches!(b, b'\n' | b'\r')) {
            line.pop();
        }
        let mut text = String::from_utf8_lossy(&line).into_owned();
        if text.len() > MAX_LINE {
            let mut cut = MAX_LINE;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push('…');
        }
        if lines.send(text).is_err() {
            return;
        }
    }
}

/// Hands lines over in batches until the program's outputs close.
fn hand_over(read: mpsc::Receiver<String>, tell: &impl Fn(Told)) {
    let mut batch = Vec::new();
    let mut last = Instant::now();
    loop {
        let line = read.recv_timeout(BATCH_EVERY);
        let ended = matches!(line, Err(mpsc::RecvTimeoutError::Disconnected));
        if let Ok(line) = line {
            batch.push(line);
        }
        let due = ended || batch.len() >= BATCH_LINES || last.elapsed() >= BATCH_EVERY;
        if due && !batch.is_empty() {
            tell(Told::Lines(std::mem::take(&mut batch)));
            last = Instant::now();
        }
        if ended {
            return;
        }
    }
}

fn end(child: &Arc<Mutex<Child>>) {
    let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
    // The whole group it leads (a shell's commands, a build's compilers).
    #[cfg(unix)]
    if child.try_wait().ok().flatten().is_none() {
        // Safety: a plain signal to a process group this module started.
        unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    }
    crate::process::kill_tree(&mut child);
}

/// Ends `extension`'s program `name`, if it's running. It's forgotten at
/// once (a program can start under that name right away) and its owner hears
/// nothing more of it: not its last lines, not its end.
pub fn kill(extension: &str, name: &str) {
    let gone = running().remove(&(extension.to_string(), name.to_string()));
    if let Some(running) = gone {
        running.ended.set();
        end(&running.child);
    }
}

/// Ends the programs `which` picks, by (extension, window).
pub fn kill_where(which: impl Fn(&str, u64) -> bool) {
    let children: Vec<Arc<Mutex<Child>>> = running().iter().filter(|((extension, _), r)| which(extension, r.window)).map(|(_, r)| r.child.clone()).collect();
    for child in children {
        end(&child);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn collect(extension: &str, name: &str, script: &str) -> mpsc::Receiver<Told> {
        let (send, told) = mpsc::channel();
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        start(extension, name, 7, command, Ended::default(), move |t| {
            send.send(t).ok();
        })
        .unwrap();
        told
    }

    fn until_exit(told: &mpsc::Receiver<Told>) -> (Vec<String>, i32) {
        let mut lines = Vec::new();
        loop {
            match told.recv_timeout(Duration::from_secs(20)).expect("the program's end") {
                Told::Lines(batch) => lines.extend(batch),
                Told::Exited(status) => return (lines, status),
            }
        }
    }

    #[test]
    fn output_comes_in_batches_then_the_exit() {
        let told = collect("test-streams", "talk", "echo one; echo two >&2; sleep 0.3; printf 'thr\\377ee\\n'; exit 3");
        // What it printed first arrives while it still runs.
        let first = told.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(matches!(&first, Told::Lines(lines) if !lines.is_empty()), "{first:?}");
        let (mut lines, status) = until_exit(&told);
        if let Told::Lines(batch) = first {
            lines.splice(0..0, batch);
        }
        lines.sort();
        assert_eq!(lines, ["one", "thr\u{fffd}ee", "two"]);
        assert_eq!(status, 3);
        assert!(!running().contains_key(&("test-streams".into(), "talk".into())));
    }

    #[test]
    fn a_program_is_ended_with_what_it_started() {
        // The shell starts a sleep of its own: both go, or the pipe would stay open for a minute.
        let told = collect("test-kill", "wait", "echo started; sleep 60 & wait");
        assert_eq!(told.recv_timeout(Duration::from_secs(20)).unwrap(), Told::Lines(vec!["started".into()]));
        let again = start("test-kill", "wait", 7, Command::new("true"), Ended::default(), |_| {});
        assert!(again.unwrap_err().contains("already running"));
        let started = Instant::now();
        kill_where(|extension, window| extension == "test-kill" && window == 7);
        let (lines, status) = until_exit(&told);
        assert!(lines.is_empty() && status == -1, "{lines:?} {status}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_killed_program_is_gone_at_once_and_says_nothing_more() {
        let first = Ended::default();
        let (send, told) = mpsc::channel();
        let mut command = Command::new("sh");
        command.args(["-c", "while true; do echo old; sleep 0.01; done"]);
        start("test-replace", "follow", 7, command, first.clone(), move |t| {
            send.send(t).ok();
        })
        .unwrap();
        assert!(matches!(told.recv_timeout(Duration::from_secs(20)).unwrap(), Told::Lines(_)));
        kill("test-replace", "follow");
        assert!(first.is_set());
        // The name is free right away, for the program that replaces it.
        let replaced = collect("test-replace", "follow", "echo new");
        let (lines, status) = until_exit(&replaced);
        assert_eq!((lines, status), (vec!["new".to_string()], 0));
        // Nothing of the old one came after the kill: no last lines, no end.
        std::thread::sleep(Duration::from_millis(300));
        while let Ok(t) = told.try_recv() {
            assert!(matches!(t, Told::Lines(_)), "{t:?} after the kill");
        }
        assert!(told.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn fast_output_is_few_batches() {
        let told = collect("test-fast", "count", "i=0; while [ $i -lt 3000 ]; do echo line $i; i=$((i+1)); done");
        let mut batches = 0;
        let mut lines = 0;
        loop {
            match told.recv_timeout(Duration::from_secs(30)).unwrap() {
                Told::Lines(batch) => {
                    assert!(batch.len() <= BATCH_LINES);
                    batches += 1;
                    lines += batch.len();
                }
                Told::Exited(status) => {
                    assert_eq!(status, 0);
                    break;
                }
            }
        }
        assert_eq!(lines, 3000);
        assert!(batches < 300, "{batches} batches for 3000 lines");
    }
}
