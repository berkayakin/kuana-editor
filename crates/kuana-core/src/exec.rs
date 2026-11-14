//! Programs run for the project (Git, formatters, linters, language
//! servers…), on the host the project is on: like `std::process`, with the
//! host chosen by the command's folder (`current_dir`). A folder under a
//! remote mount (`crate::fs`) runs it on that host through its `Host`, with
//! mount paths in the arguments turned into the host's own paths and Kuana's
//! own `PATH` left out (the remote's login environment has its own).

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::fs;

/// What the remote side runs (`Host::spawn`): the host's own paths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Variables to set (`Some`) or remove (`None`).
    pub env: Vec<(String, Option<String>)>,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

/// A remote program, as `Host::spawn` started it.
pub struct Spawned {
    pub stdin: Option<Box<dyn Write + Send>>,
    pub stdout: Option<Box<dyn Read + Send>>,
    pub stderr: Option<Box<dyn Read + Send>>,
    pub process: Box<dyn RemoteProcess>,
}

pub trait RemoteProcess: Send {
    fn id(&self) -> u32;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
}

/// Hosts that run programs (`crate::fs::Host` plus this).
pub trait Runner: Send + Sync {
    fn spawn(&self, spec: &Spec) -> io::Result<Spawned>;
}

/// How a host's `Runner` is found for a mount: `crate::fs::mount` keeps
/// hosts as `fs::Host`; runners are registered beside them.
static RUNNERS: std::sync::RwLock<Vec<(PathBuf, Arc<dyn Runner>)>> = std::sync::RwLock::new(Vec::new());

/// Registers the program runner for the host mounted at `root`.
pub fn mount_runner(root: &Path, runner: Arc<dyn Runner>) {
    let mut runners = RUNNERS.write().unwrap_or_else(|e| e.into_inner());
    runners.retain(|(r, _)| r != root);
    runners.push((root.to_path_buf(), runner));
}

pub fn unmount_runner(root: &Path) {
    RUNNERS.write().unwrap_or_else(|e| e.into_inner()).retain(|(r, _)| r != root);
}

fn runner_for(mount: &Path) -> Option<Arc<dyn Runner>> {
    RUNNERS.read().unwrap_or_else(|e| e.into_inner()).iter().find(|(r, _)| r == mount).map(|(_, runner)| runner.clone())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Io {
    Null,
    Piped,
    Inherit,
}

/// `std::process::Stdio`, which can't be looked into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stdio(Io);

impl Stdio {
    pub fn null() -> Self {
        Stdio(Io::Null)
    }

    pub fn piped() -> Self {
        Stdio(Io::Piped)
    }

    pub fn inherit() -> Self {
        Stdio(Io::Inherit)
    }

    fn to_std(self) -> std::process::Stdio {
        match self.0 {
            Io::Null => std::process::Stdio::null(),
            Io::Piped => std::process::Stdio::piped(),
            Io::Inherit => std::process::Stdio::inherit(),
        }
    }
}

/// `std::process::Command`, run on the project's host.
#[derive(Clone, Debug)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(OsString, Option<OsString>)>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Command { program: program.as_ref().to_owned(), args: Vec::new(), cwd: None, env: Vec::new(), stdin: None, stdout: None, stderr: None }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&mut self, args: I) -> &mut Self {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env.push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn envs<I: IntoIterator<Item = (K, V)>, K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, vars: I) -> &mut Self {
        for (k, v) in vars {
            self.env(k, v);
        }
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.push((key.as_ref().to_owned(), None));
        self
    }

    pub fn stdin(&mut self, io: Stdio) -> &mut Self {
        self.stdin = Some(io);
        self
    }

    pub fn stdout(&mut self, io: Stdio) -> &mut Self {
        self.stdout = Some(io);
        self
    }

    pub fn stderr(&mut self, io: Stdio) -> &mut Self {
        self.stderr = Some(io);
        self
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, Option<&OsStr>)> {
        self.env.iter().map(|(k, v)| (k.as_os_str(), v.as_deref()))
    }

    /// Where it runs: a remote host's mount (None: this computer).
    fn mount(&self) -> Option<PathBuf> {
        self.cwd.as_deref().and_then(fs::mount_of)
    }

    /// As `std::process::Command`, for this computer (no window on Windows).
    pub fn to_std(&self) -> std::process::Command {
        let mut command = crate::process::command(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        for (k, v) in &self.env {
            match v {
                Some(v) => command.env(k, v),
                None => command.env_remove(k),
            };
        }
        for (io, set) in [(self.stdin, 0), (self.stdout, 1), (self.stderr, 2)] {
            if let Some(io) = io {
                match set {
                    0 => command.stdin(io.to_std()),
                    1 => command.stdout(io.to_std()),
                    _ => command.stderr(io.to_std()),
                };
            }
        }
        command
    }

    /// As the host under `mount` runs it: its own paths, without our `PATH`.
    pub fn to_spec(&self, mount: &Path) -> Spec {
        let host = |s: &OsStr| -> String {
            let path = Path::new(s);
            match path.strip_prefix(mount) {
                Ok(rest) => fs::local_to_host(rest),
                Err(_) => s.to_string_lossy().into_owned(),
            }
        };
        Spec {
            program: host(&self.program),
            args: self.args.iter().map(|a| host(a)).collect(),
            cwd: self.cwd.as_deref().map(|c| host(c.as_os_str())),
            env: self
                .env
                .iter()
                .filter(|(k, _)| k != "PATH")
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.as_ref().map(|v| host(v))))
                .collect(),
            stdin: self.stdin.is_some_and(|s| s.0 == Io::Piped),
            stdout: self.stdout.is_some_and(|s| s.0 == Io::Piped),
            stderr: self.stderr.is_some_and(|s| s.0 == Io::Piped),
        }
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        match self.mount() {
            None => {
                let mut child = self.to_std().spawn()?;
                Ok(Child {
                    stdin: child.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>),
                    stdout: child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
                    stderr: child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
                    process: Process::Local(child),
                })
            }
            Some(mount) => {
                let runner = runner_for(&mount).ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "the remote host isn't connected"))?;
                let spawned = runner.spawn(&self.to_spec(&mount))?;
                Ok(Child { stdin: spawned.stdin, stdout: spawned.stdout, stderr: spawned.stderr, process: Process::Remote(spawned.process) })
            }
        }
    }

    /// Runs it to the end, collecting its output (stdin null unless set).
    pub fn output(&mut self) -> io::Result<Output> {
        if self.mount().is_none() {
            let mut command = self.to_std();
            if self.stdin.is_none() {
                command.stdin(std::process::Stdio::null());
            }
            let output = command.output()?;
            return Ok(Output { status: ExitStatus { code: output.status.code() }, stdout: output.stdout, stderr: output.stderr });
        }
        self.stdout(Stdio::piped()).stderr(Stdio::piped());
        if self.stdin.is_none() {
            self.stdin(Stdio::null());
        }
        self.spawn()?.wait_with_output()
    }

    pub fn status(&mut self) -> io::Result<ExitStatus> {
        if self.stdout.is_none() {
            self.stdout(Stdio::null());
        }
        if self.stderr.is_none() {
            self.stderr(Stdio::null());
        }
        self.spawn()?.wait()
    }
}

/// How a program ended; `code` is None when a signal ended it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExitStatus {
    pub code: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn code(&self) -> Option<i32> {
        self.code
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

enum Process {
    Local(std::process::Child),
    Remote(Box<dyn RemoteProcess>),
}

/// `std::process::Child`, on either host.
pub struct Child {
    pub stdin: Option<Box<dyn Write + Send>>,
    pub stdout: Option<Box<dyn Read + Send>>,
    pub stderr: Option<Box<dyn Read + Send>>,
    process: Process,
}

impl Child {
    pub fn id(&self) -> u32 {
        match &self.process {
            Process::Local(c) => c.id(),
            Process::Remote(p) => p.id(),
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        match &mut self.process {
            Process::Local(c) => c.kill(),
            Process::Remote(p) => p.kill(),
        }
    }

    /// Ends it and what it started (`process::kill_tree` for a local one).
    pub fn kill_tree(&mut self) {
        match &mut self.process {
            Process::Local(c) => crate::process::kill_tree(c),
            Process::Remote(p) => {
                let _ = p.kill();
            }
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        match &mut self.process {
            Process::Local(c) => c.wait().map(|s| ExitStatus { code: s.code() }),
            Process::Remote(p) => p.wait(),
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.process {
            Process::Local(c) => c.try_wait().map(|s| s.map(|s| ExitStatus { code: s.code() })),
            Process::Remote(p) => p.try_wait(),
        }
    }

    /// Waits, reading stdout and stderr to their ends (on threads, so a full
    /// pipe can't stall the program).
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let read = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut out = Vec::new();
                if let Some(mut pipe) = pipe {
                    pipe.read_to_end(&mut out)?;
                }
                Ok::<_, io::Error>(out)
            })
        };
        let (stdout, stderr) = (read(self.stdout.take()), read(self.stderr.take()));
        let status = self.wait()?;
        let join = |t: std::thread::JoinHandle<io::Result<Vec<u8>>>| t.join().unwrap_or_else(|_| Err(io::Error::other("reader panicked")));
        Ok(Output { status, stdout: join(stdout)?, stderr: join(stderr)? })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A runner that answers with what it was asked to run.
    #[derive(Default)]
    struct Echo(Mutex<Vec<Spec>>);

    struct Done;

    impl RemoteProcess for Done {
        fn id(&self) -> u32 {
            7
        }
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus { code: Some(3) })
        }
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(Some(ExitStatus { code: Some(3) }))
        }
    }

    impl Runner for Echo {
        fn spawn(&self, spec: &Spec) -> io::Result<Spawned> {
            self.0.lock().unwrap().push(spec.clone());
            let said = format!("{} {}", spec.program, spec.args.join(" "));
            Ok(Spawned { stdin: None, stdout: Some(Box::new(io::Cursor::new(said.into_bytes()))), stderr: None, process: Box::new(Done) })
        }
    }

    #[test]
    fn programs_run_where_their_folder_is() {
        // Here: as std::process does.
        let out = Command::new(if cfg!(windows) { "cmd" } else { "echo" })
            .args(if cfg!(windows) { vec!["/C", "echo hi"] } else { vec!["hi"] })
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");

        // There: the host's paths, without our PATH.
        let echo = Arc::new(Echo::default());
        let root = fs::mount_root("exec-test-host");
        mount_runner(&root, echo.clone());
        let project = root.join("home").join("me").join("p");
        let mut command = Command::new("git");
        command.args(["-C".as_ref(), project.as_os_str(), "status".as_ref()]).current_dir(&project).env("PATH", "/local/bin").env("GIT_X", "1").env_remove("GIT_DIR");
        // Not mounted as a file host: no mount, so it runs here; mount it.
        let fs_root = fs::mount("exec-test-host", Arc::new(NoFiles));
        assert_eq!(fs_root, root);
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "git -C /home/me/p status");
        let spec = echo.0.lock().unwrap()[0].clone();
        assert_eq!(spec.cwd.as_deref(), Some("/home/me/p"));
        assert_eq!(spec.env, [("GIT_X".to_string(), Some("1".to_string())), ("GIT_DIR".to_string(), None)]);
        assert!(spec.stdout && spec.stderr && !spec.stdin);
        fs::unmount(&root);
        unmount_runner(&root);
    }

    /// A host for `fs::mount` with no files.
    struct NoFiles;

    impl fs::Host for NoFiles {
        fn read(&self, _: &str) -> io::Result<Vec<u8>> {
            Err(io::ErrorKind::NotFound.into())
        }
        fn write(&self, _: &str, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn create(&self, _: &str, _: &[u8], _: bool) -> io::Result<()> {
            Ok(())
        }
        fn metadata(&self, _: &str, _: bool) -> io::Result<fs::HostMetadata> {
            Err(io::ErrorKind::NotFound.into())
        }
        fn read_dir(&self, _: &str) -> io::Result<Vec<fs::HostEntry>> {
            Ok(Vec::new())
        }
        fn create_dir(&self, _: &str, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn remove_file(&self, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn remove_dir(&self, _: &str, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn rename(&self, _: &str, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn copy(&self, _: &str, _: &str) -> io::Result<u64> {
            Ok(0)
        }
        fn canonicalize(&self, p: &str) -> io::Result<String> {
            Ok(p.to_string())
        }
        fn read_link(&self, _: &str) -> io::Result<String> {
            Err(io::ErrorKind::InvalidInput.into())
        }
        fn trash(&self, p: &str) -> io::Result<String> {
            Ok(p.to_string())
        }
    }
}
