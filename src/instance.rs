//! One Kuana per config folder, on every platform: a second
//! launch hands its paths to the running Kuana and exits, so two processes
//! never rewrite the same sessions and state (the Windows jump list and the
//! `kuana` command outside an app bundle start a new process each time; a
//! macOS app bundle gets this from the system already). The running Kuana
//! listens on a loopback port that it writes, with a random token, to
//! `<config>/instance.json` (readable by its user only), and holds a lock on
//! `<config>/instance.lock` for as long as it runs: a launch that finds the
//! lock held never starts as a second running Kuana, even when the first is
//! still starting or slow to answer.
//!
//! Only release builds do this: development
//! builds and their tests often run several instances on purpose.

use std::{
    fs::{File, TryLockError},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// What `instance.json` holds.
#[derive(Serialize, Deserialize)]
struct Record {
    port: u16,
    token: String,
}

/// What a second launch sends.
#[derive(Serialize, Deserialize)]
struct Hello {
    token: String,
    paths: Vec<PathBuf>,
}

/// This process is the running Kuana: it takes later launches' paths.
pub struct Primary {
    listener: TcpListener,
    token: String,
    /// `instance.lock`, held (None where locking isn't supported).
    lock: Option<File>,
}

/// How long a launch tries to reach the Kuana holding the lock (it may be
/// starting, or busy) before giving up; it never starts as a second one.
const REACH_WAIT: Duration = Duration::from_secs(8);
/// How long the other side has to say its line, in all.
const LINE_WAIT: Duration = Duration::from_secs(3);
/// The longest line read: a launch's paths, not a stream to make room for.
const MAX_LINE: usize = 1 << 20;

/// Whether to go on starting: `Ok(Some)` as the running Kuana, `Ok(None)`
/// without the single-instance check (a development build), `Err(())` when
/// the paths went to the Kuana already running (this one exits).
pub fn claim(paths: &[PathBuf]) -> Result<Option<Primary>, ()> {
    let enforced = option_env!("KUANA_RELEASE").is_some() || std::env::var_os("KUANA_SINGLE_INSTANCE").is_some();
    match crate::theme::config_dir() {
        Some(dir) if enforced => claim_in(&dir, paths),
        _ => Ok(None),
    }
}

fn claim_in(dir: &Path, paths: &[PathBuf]) -> Result<Option<Primary>, ()> {
    claim_within(dir, paths, REACH_WAIT)
}

fn claim_within(dir: &Path, paths: &[PathBuf], wait: Duration) -> Result<Option<Primary>, ()> {
    let file = dir.join("instance.json");
    // Relative paths mean this launch's folder, which the running Kuana
    // doesn't know.
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths: Vec<PathBuf> = paths.iter().map(|p| cwd.join(p)).collect();
    let _ = std::fs::create_dir_all(dir);
    let lock = File::options().create(true).truncate(false).write(true).open(dir.join("instance.lock"));
    let lock = match lock {
        Ok(lock) => lock,
        // Not even a lock file: as before locks, whoever answers runs.
        Err(_) => return forward_or_start(&file, &paths),
    };
    let deadline = Instant::now() + wait;
    loop {
        match lock.try_lock() {
            // Nobody's running (a record left is a crashed one's): this is the one.
            Ok(()) => return Ok(start(&file, Some(lock))),
            Err(TryLockError::WouldBlock) => {}
            // Locks unsupported here (some network drives).
            Err(TryLockError::Error(_)) => return forward_or_start(&file, &paths),
        }
        // Running: it takes the paths, once it's written where it listens.
        if read_record(&file).is_some_and(|record| forward(&record, &paths).is_ok()) {
            return Err(());
        }
        if Instant::now() >= deadline {
            eprintln!("Kuana is already running but didn't answer; try again in a moment");
            return Err(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_record(file: &Path) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Without a lock: the paths go to whoever answers at `file`'s port, else
/// this launch runs.
fn forward_or_start(file: &Path, paths: &[PathBuf]) -> Result<Option<Primary>, ()> {
    if read_record(file).is_some_and(|record| forward(&record, paths).is_ok()) {
        return Err(());
    }
    Ok(start(file, None))
}

/// Starts listening as the running Kuana and writes where to `file`.
fn start(file: &Path, lock: Option<File>) -> Option<Primary> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    let token = random_token();
    let record = Record { port: listener.local_addr().map(|a| a.port()).unwrap_or(0), token: token.clone() };
    let json = serde_json::to_string(&record).unwrap_or_default();
    if let Err(err) = write_private(file, json.as_bytes()) {
        eprintln!("couldn't note the running Kuana: {err}");
    }
    Some(Primary { listener, token, lock })
}

/// Writes `file` readable by its user only (it holds the token), through a
/// temporary file renamed over it, so a launch never reads half of it.
fn write_private(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = file.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let mut options = File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temp).and_then(|mut f| {
        // A temporary file left by an earlier process with this id keeps its mode.
        #[cfg(unix)]
        f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(err) = written.and_then(|_| std::fs::rename(&temp, file)) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// Hands `paths` to the Kuana in `record`; Ok once it confirmed.
fn forward(record: &Record, paths: &[PathBuf]) -> std::io::Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, record.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    let hello = Hello { token: record.token.clone(), paths: paths.to_vec() };
    stream.write_all((serde_json::to_string(&hello)? + "\n").as_bytes())?;
    let answer = read_line(&mut stream, LINE_WAIT).ok_or_else(|| std::io::Error::other("no answer"))?;
    if answer.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("refused")) }
}

/// One line from `stream` (without its line break): None if it doesn't
/// come within `wait` all told, however slowly it trickles in, or is longer
/// than `MAX_LINE`.
fn read_line(stream: &mut TcpStream, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    let mut line = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        let read = stream.read(&mut chunk).ok().filter(|&n| n > 0)?;
        line.extend_from_slice(&chunk[..read]);
        if let Some(end) = line.iter().position(|&b| b == b'\n') {
            line.truncate(end);
            return String::from_utf8(line).ok();
        }
        if line.len() > MAX_LINE {
            return None;
        }
    }
}

impl Primary {
    /// Takes later launches' paths for as long as Kuana runs; an empty list
    /// means a launch without paths (bring Kuana to the front).
    pub fn serve(self, on_paths: impl Fn(Vec<PathBuf>) + Send + 'static) {
        let Primary { listener, token, lock } = self;
        // Held until the process exits, which releases it.
        std::mem::forget(lock);
        let spawned = std::thread::Builder::new().name("instance".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Some(paths) = receive(stream, &token) {
                    on_paths(paths);
                }
            }
        });
        if let Err(err) = spawned {
            eprintln!("couldn't listen for other launches: {err}");
        }
    }
}

/// One launch's paths, if it knew the token.
fn receive(mut stream: TcpStream, token: &str) -> Option<Vec<PathBuf>> {
    let hello: Hello = serde_json::from_str(&read_line(&mut stream, LINE_WAIT)?).ok()?;
    if hello.token != token {
        return None;
    }
    stream.write_all(b"ok\n").ok()?;
    Some(hello.paths)
}

/// A token another local program can't guess (std's hasher keys are random).
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    (0..2)
        .map(|i| {
            let mut hasher = state.build_hasher();
            hasher.write_u64(i ^ std::process::id() as u64);
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_hands_its_paths_to_the_first() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = claim_in(&dir, &[]).unwrap().expect("first launch runs");
        let (tx, rx) = std::sync::mpsc::channel();
        first.serve(move |paths| tx.send(paths).unwrap());
        // The second one forwards and is told to exit. (An absolute path on
        // every system: `/work/a.rs` has no drive on Windows, so it gets the
        // current one.)
        let file = std::env::temp_dir().join("work").join("a.rs");
        assert!(claim_in(&dir, std::slice::from_ref(&file)).is_err());
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), [file]);
        // A wrong token is ignored, and the launch starts on its own.
        let record: Record = serde_json::from_str(&std::fs::read_to_string(dir.join("instance.json")).unwrap()).unwrap();
        assert!(forward(&Record { token: "guess".into(), ..record }, &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A port nothing listens on (bound, then released).
    fn unused_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
    }

    /// A launch while the running Kuana is still starting (where it listens
    /// not written yet) waits for it rather than starting too.
    #[test]
    fn a_launch_waits_for_a_starting_kuana() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-starting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = claim_in(&dir, &[]).unwrap().expect("first launch runs");
        let record = std::fs::read_to_string(dir.join("instance.json")).unwrap();
        // A crashed one's record, not the first's yet.
        std::fs::write(dir.join("instance.json"), format!(r#"{{"port":{},"token":"t"}}"#, unused_port())).unwrap();
        let file = std::env::temp_dir().join("work").join("b.rs");
        let second = {
            let (dir, file) = (dir.clone(), file.clone());
            std::thread::spawn(move || claim_in(&dir, &[file]).is_err())
        };
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join("instance.json"), record).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        first.serve(move |paths| tx.send(paths).unwrap());
        assert!(second.join().unwrap(), "the second launch started too");
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), [file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While the lock is held, a launch the running Kuana doesn't answer
    /// gives up; it never runs beside it.
    #[test]
    fn a_launch_never_runs_beside_a_kuana_that_doesnt_answer() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-silent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = claim_in(&dir, &[]).unwrap().expect("first launch runs");
        std::fs::write(dir.join("instance.json"), format!(r#"{{"port":{},"token":"t"}}"#, unused_port())).unwrap();
        assert!(claim_within(&dir, &[], Duration::from_millis(300)).is_err());
        // Gone (its lock with it): the next launch runs.
        drop(first);
        assert!(claim_within(&dir, &[], Duration::from_millis(300)).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record holds the token: only its user may read it.
    #[cfg(unix)]
    #[test]
    fn the_record_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kuana-instance-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("instance.json"), "{}").unwrap();
        let _first = claim_in(&dir, &[]).unwrap().expect("first launch runs");
        let mode = std::fs::metadata(dir.join("instance.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line too long, or trickling in for too long, isn't waited for.
    #[test]
    fn lines_are_read_within_limits() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let line_from = |send: Box<dyn FnOnce(TcpStream) + Send>, wait: Duration| {
            let client = TcpStream::connect(address).unwrap();
            let sending = std::thread::spawn(move || send(client));
            let (mut stream, _) = listener.accept().unwrap();
            let started = Instant::now();
            let line = read_line(&mut stream, wait);
            drop(stream);
            sending.join().ok();
            (line, started.elapsed())
        };
        let (line, _) = line_from(Box::new(|mut c| c.write_all(b"hello\nmore").unwrap()), LINE_WAIT);
        assert_eq!(line.as_deref(), Some("hello"));
        let endless = |mut c: TcpStream| {
            let chunk = [b'x'; 64 << 10];
            while c.write_all(&chunk).is_ok() {}
        };
        assert_eq!(line_from(Box::new(endless), LINE_WAIT).0, None);
        let trickle = |mut c: TcpStream| {
            while c.write_all(b"x").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let (line, took) = line_from(Box::new(trickle), Duration::from_millis(300));
        assert_eq!(line, None);
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn a_stale_record_doesnt_stop_a_launch() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A port nothing listens on (bound, then released).
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        std::fs::write(dir.join("instance.json"), format!(r#"{{"port":{port},"token":"t"}}"#)).unwrap();
        assert!(claim_in(&dir, &[]).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
