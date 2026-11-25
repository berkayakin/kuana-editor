//! The terminals agents run commands in (ACP's `terminal/*`): processes
//! Kuana starts with their output captured (the tail, within the agent's
//! byte limit), shown in the tool call that ran them and kept after the
//! agent releases them.

use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use futures::channel::oneshot;
use serde_json::{Value, json};

/// Output kept when the agent names no limit.
const DEFAULT_LIMIT: usize = 1024 * 1024;
/// Finished terminals kept (their output shows in the session's cards);
/// older ones are let go.
const KEPT: usize = 40;
/// How long the end waits for the last output once the process exited (a
/// child it left in the background may hold the pipes open).
const DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

struct Proc {
    child: Mutex<Option<Child>>,
    output: Mutex<String>,
    limit: usize,
    truncated: AtomicBool,
    exit: Mutex<Option<Exit>>,
    waiters: Mutex<Vec<oneshot::Sender<Exit>>>,
}

impl Proc {
    fn push(&self, text: &str) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.push_str(text);
        if output.len() > self.limit {
            let mut cut = output.len() - self.limit;
            while !output.is_char_boundary(cut) {
                cut += 1;
            }
            output.drain(..cut);
            self.truncated.store(true, Ordering::Relaxed);
        }
    }

    fn finish(&self, exit: Exit) {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(exit);
        for waiter in self.waiters.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            let _ = waiter.send(exit);
        }
    }
}

/// A session's terminals, by the id Kuana gave them.
#[derive(Default)]
pub struct Terminals {
    procs: HashMap<String, Arc<Proc>>,
    /// Ids oldest first, and the last number given.
    order: Vec<String>,
    next: usize,
    /// Bumped whenever any output arrives or a process ends, so the view
    /// knows to redraw.
    pub generation: Arc<AtomicU64>,
}

impl Terminals {
    /// Starts `command` (with `args`; a lone command line runs in the
    /// shell); its id.
    pub fn create(&mut self, command: &str, args: &[String], cwd: &Path, env: &[(String, String)], limit: Option<usize>) -> Result<String, String> {
        let (program, args): (String, Vec<String>) = if args.is_empty() && command.contains(char::is_whitespace) {
            if cfg!(windows) {
                ("powershell".into(), vec!["-NoProfile".into(), "-Command".into(), command.into()])
            } else {
                ("sh".into(), vec!["-c".into(), command.into()])
            }
        } else {
            (command.into(), args.to_vec())
        };
        let mut cmd = crate::process::command(&program);
        cmd.args(&args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Its own process group, so killing it reaches a pipeline's commands too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        cmd.env("PATH", crate::lsp::servers::search_path());
        for (name, value) in env {
            cmd.env(name, value);
        }
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't run {program}: {e}"))?;
        let proc = Arc::new(Proc {
            child: Mutex::new(None),
            output: Mutex::new(String::new()),
            limit: limit.filter(|l| *l > 0).unwrap_or(DEFAULT_LIMIT),
            truncated: AtomicBool::new(false),
            exit: Mutex::new(None),
            waiters: Mutex::new(Vec::new()),
        });
        let (drained, all_drained) = std::sync::mpsc::channel::<()>();
        let readers = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
            .into_iter()
            .flatten()
            .filter_map(|mut pipe| {
                let (proc, generation, drained) = (proc.clone(), self.generation.clone(), drained.clone());
                std::thread::Builder::new()
                    .name("agent terminal".into())
                    .spawn(move || {
                        let mut buf = [0u8; 8192];
                        let mut carry = Vec::new();
                        while let Ok(n) = pipe.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            carry.extend_from_slice(&buf[..n]);
                            proc.push(&take_utf8(&mut carry));
                            generation.fetch_add(1, Ordering::Relaxed);
                        }
                        if !carry.is_empty() {
                            proc.push(&String::from_utf8_lossy(&carry));
                        }
                        let _ = drained.send(());
                    })
                    .ok()
            })
            .count();
        drop(drained);
        *proc.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        // The waiter polls, so `kill` can take the child meanwhile.
        let (waiting, generation) = (proc.clone(), self.generation.clone());
        let _ = std::thread::Builder::new().name("agent terminal wait".into()).spawn(move || {
            let exit = loop {
                let status = {
                    let mut child = waiting.child.lock().unwrap_or_else(|e| e.into_inner());
                    match child.as_mut() {
                        Some(c) => c.try_wait().ok().flatten(),
                        None => break Exit { code: None, signal: Some(9) },
                    }
                };
                if let Some(status) = status {
                    break exit_of(status);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            };
            // All output first, then the end; but not for ever, when a child
            // left behind keeps the pipes open.
            let until = std::time::Instant::now() + DRAIN;
            for _ in 0..readers {
                if all_drained.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).is_err() {
                    break;
                }
            }
            waiting.finish(exit);
            generation.fetch_add(1, Ordering::Relaxed);
        });
        self.next += 1;
        let id = format!("term-{}", self.next);
        self.procs.insert(id.clone(), proc);
        self.order.push(id.clone());
        self.prune();
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }

    /// `terminal/output`'s answer.
    pub fn output(&self, id: &str) -> Option<Value> {
        let proc = self.procs.get(id)?;
        let output = proc.output.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let exit = *proc.exit.lock().unwrap_or_else(|e| e.into_inner());
        let mut answer = json!({ "output": output, "truncated": proc.truncated.load(Ordering::Relaxed) });
        if let Some(exit) = exit {
            answer["exitStatus"] = json!({ "exitCode": exit.code, "signal": exit.signal.map(|s| s.to_string()) });
        }
        Some(answer)
    }

    /// The last `lines` lines of the output so far (without copying the
    /// rest) and how it ended.
    pub fn tail(&self, id: &str, lines: usize) -> Option<(String, Option<Exit>)> {
        let proc = self.procs.get(id)?;
        let output = proc.output.lock().unwrap_or_else(|e| e.into_inner());
        let trimmed = output.trim_end_matches('\n');
        let start = trimmed.rmatch_indices('\n').nth(lines.saturating_sub(1)).map_or(0, |(at, _)| at + 1);
        let tail = trimmed[start..].to_string();
        drop(output);
        Some((tail, *proc.exit.lock().unwrap_or_else(|e| e.into_inner())))
    }

    /// Resolves when the process ends.
    pub fn wait(&self, id: &str) -> Option<oneshot::Receiver<Exit>> {
        let proc = self.procs.get(id)?;
        let (tx, rx) = oneshot::channel();
        match *proc.exit.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(exit) => {
                let _ = tx.send(exit);
            }
            None => proc.waiters.lock().unwrap_or_else(|e| e.into_inner()).push(tx),
        }
        Some(rx)
    }

    pub fn kill(&self, id: &str) -> bool {
        let Some(proc) = self.procs.get(id) else { return false };
        if let Some(mut child) = proc.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // The whole group it leads (a pipeline, what a script started).
            #[cfg(unix)]
            if child.try_wait().ok().flatten().is_none() {
                // Safety: a plain signal to a process group this module started.
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
            }
            crate::process::kill_tree(&mut child);
            proc.finish(Exit { code: None, signal: Some(9) });
        }
        true
    }

    /// Lets go of the oldest finished terminals past `KEPT`.
    fn prune(&mut self) {
        while self.order.len() > KEPT {
            let Some(at) = self.order.iter().position(|id| self.procs.get(id).is_some_and(|p| p.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some())) else { break };
            let id = self.order.remove(at);
            self.procs.remove(&id);
        }
    }

    pub fn running(&self) -> bool {
        self.procs.values().any(|p| p.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none())
    }

    /// Stops every process (the session closed).
    pub fn kill_all(&self) {
        for id in self.procs.keys() {
            self.kill(id);
        }
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// The text in `bytes` up to an incomplete character at the end, which stays
/// for the next read; bytes that aren't UTF-8 become U+FFFD.
fn take_utf8(bytes: &mut Vec<u8>) -> String {
    let mut text = String::new();
    let mut rest: &[u8] = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match e.error_len() {
                    Some(bad) => {
                        text.push('\u{FFFD}');
                        rest = &after[bad..];
                    }
                    // Cut off mid-character: the rest comes with the next read.
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    let kept = rest.to_vec();
    *bytes = kept;
    text
}

fn exit_of(status: std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    Exit { code: status.code(), signal }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn characters_split_across_reads_stay_whole() {
        let mut carry = "héllo".as_bytes()[..2].to_vec();
        assert_eq!(take_utf8(&mut carry), "h");
        carry.extend_from_slice(&"héllo".as_bytes()[2..]);
        assert_eq!(take_utf8(&mut carry), "éllo");
        let mut bad = b"a\xffb".to_vec();
        assert_eq!(take_utf8(&mut bad), "a\u{FFFD}b");
        assert!(bad.is_empty());
    }

    #[test]
    fn a_pipeline_dies_with_its_shell() {
        let mut terminals = Terminals::default();
        let id = terminals.create("sleep 30 | cat", &[], &std::env::temp_dir(), &[], None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert!(terminals.kill(&id));
        futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        assert!(!terminals.running());
        // A background child holding the pipe doesn't hold up the end.
        let id = terminals.create("sleep 30 & echo started", &[], &std::env::temp_dir(), &[], None).unwrap();
        let exit = futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        assert_eq!(exit.code, Some(0));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        terminals.kill_all();
    }

    #[test]
    fn output_is_captured_and_waited_for() {
        let mut terminals = Terminals::default();
        let id = terminals.create("printf 'one\\ntwo' && exit 3", &[], &std::env::temp_dir(), &[], None).unwrap();
        let exit = futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        assert_eq!(exit.code, Some(3));
        let answer = terminals.output(&id).unwrap();
        assert_eq!(answer["output"], "one\ntwo");
        assert_eq!(terminals.tail(&id, 1).unwrap().0, "two");
        assert_eq!(terminals.tail(&id, 5).unwrap().0, "one\ntwo");
        assert_eq!(answer["exitStatus"]["exitCode"], 3);
        assert!(!terminals.running());
    }

    #[test]
    fn long_output_keeps_its_tail_and_kill_ends_it() {
        let mut terminals = Terminals::default();
        let id = terminals.create("printf", &["abcdefghij".into()], &std::env::temp_dir(), &[], Some(4)).unwrap();
        futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        let answer = terminals.output(&id).unwrap();
        assert_eq!((answer["output"].as_str(), answer["truncated"].as_bool()), (Some("ghij"), Some(true)));

        let id = terminals.create("sleep", &["30".into()], &std::env::temp_dir(), &[], None).unwrap();
        assert!(terminals.running());
        assert!(terminals.kill(&id));
        let exit = futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        assert_eq!(exit.code, None);
    }
}
