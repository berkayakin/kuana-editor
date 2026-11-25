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
    fn push(&self, bytes: &[u8]) {
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        output.push_str(&String::from_utf8_lossy(bytes));
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
        let readers: Vec<std::thread::JoinHandle<()>> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
            .into_iter()
            .flatten()
            .filter_map(|mut pipe| {
                let (proc, generation) = (proc.clone(), self.generation.clone());
                std::thread::Builder::new()
                    .name("agent terminal".into())
                    .spawn(move || {
                        let mut buf = [0u8; 8192];
                        while let Ok(n) = pipe.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            proc.push(&buf[..n]);
                            generation.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                    .ok()
            })
            .collect();
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
            // All output first, then the end.
            for reader in readers {
                let _ = reader.join();
            }
            waiting.finish(exit);
            generation.fetch_add(1, Ordering::Relaxed);
        });
        let id = format!("term-{}", self.procs.len() + 1);
        self.procs.insert(id.clone(), proc);
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

    /// What the view shows: the output so far and how it ended.
    pub fn shown(&self, id: &str) -> Option<(String, Option<Exit>)> {
        let proc = self.procs.get(id)?;
        Some((proc.output.lock().unwrap_or_else(|e| e.into_inner()).clone(), *proc.exit.lock().unwrap_or_else(|e| e.into_inner())))
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
            crate::process::kill_tree(&mut child);
            proc.finish(Exit { code: None, signal: Some(9) });
        }
        true
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
    fn output_is_captured_and_waited_for() {
        let mut terminals = Terminals::default();
        let id = terminals.create("printf 'one\\ntwo' && exit 3", &[], &std::env::temp_dir(), &[], None).unwrap();
        let exit = futures::executor::block_on(terminals.wait(&id).unwrap()).unwrap();
        assert_eq!(exit.code, Some(3));
        let answer = terminals.output(&id).unwrap();
        assert_eq!(answer["output"], "one\ntwo");
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
