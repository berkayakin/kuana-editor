//! The project's files, wherever they are. Project-side code (opening and
//! saving files, the file tree, the project scan, Git, run targets…) uses
//! these in place of `std::fs` and `Path::is_file` & co; Kuana's own files
//! (config, sessions, extensions) use `std::fs` (see `clippy.toml`).
//!
//! A remote project's files are under a mount path (`mount_root`):
//! `/kuana-remote/<host>/home/me/project` (`\\kuana-remote\<host>\…` on
//! Windows) is `/home/me/project` on that host. A path under a mount goes to
//! its `Host` (the connection to `kuana-server`); any other path to `std::fs`,
//! at the cost of one atomic load while nothing is mounted. Code that works
//! with paths keeps working with remote ones, the folder a process runs in
//! included (`crate::exec`).

use std::{
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

/// What a remote host does with files (`kuana-server`'s side of it). Paths
/// are the host's own, absolute, with `/` separators.
pub trait Host: Send + Sync {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()>;
    /// `create_new`: fail if it exists.
    fn create(&self, path: &str, data: &[u8], create_new: bool) -> io::Result<()>;
    /// `follow`: through a symlink (as `metadata`), else of it (`symlink_metadata`).
    fn metadata(&self, path: &str, follow: bool) -> io::Result<HostMetadata>;
    fn read_dir(&self, path: &str) -> io::Result<Vec<HostEntry>>;
    fn create_dir(&self, path: &str, all: bool) -> io::Result<()>;
    fn remove_file(&self, path: &str) -> io::Result<()>;
    fn remove_dir(&self, path: &str, all: bool) -> io::Result<()>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn copy(&self, from: &str, to: &str) -> io::Result<u64>;
    fn canonicalize(&self, path: &str) -> io::Result<String>;
    fn read_link(&self, path: &str) -> io::Result<String>;
    /// Moves to the host's trash (freedesktop's on Linux), returning where it
    /// went there (a rename back restores it).
    fn trash(&self, path: &str) -> io::Result<String>;
    /// The connection is up (a down one fails every call with `NotConnected`).
    fn connected(&self) -> bool {
        true
    }
}

/// What a host says about a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostMetadata {
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub len: u64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: Option<u64>,
    pub readonly: bool,
    /// Unix: any execute bit is set.
    pub executable: bool,
}

/// An entry of a host's folder.
#[derive(Clone, Debug, PartialEq)]
pub struct HostEntry {
    pub name: String,
    pub metadata: HostMetadata,
}

struct Mount {
    /// Where it is locally: `/kuana-remote/<name>`.
    root: PathBuf,
    host: Arc<dyn Host>,
}

static MOUNTS: RwLock<Vec<Mount>> = RwLock::new(Vec::new());
/// Whether anything is mounted: the local path's fast way through.
static ANY: AtomicBool = AtomicBool::new(false);

/// The folder a host named `name` is mounted at.
pub fn mount_root(name: &str) -> PathBuf {
    let name: String = name.chars().map(|c| if c.is_alphanumeric() || "-_.@".contains(c) { c } else { '_' }).collect();
    if cfg!(windows) { PathBuf::from(format!(r"\\kuana-remote\{name}\")) } else { PathBuf::from("/kuana-remote").join(name) }
}

/// Mounts `host` at `mount_root(name)`, replacing what was there (a reconnect).
pub fn mount(name: &str, host: Arc<dyn Host>) -> PathBuf {
    let root = mount_root(name);
    let mut mounts = MOUNTS.write().unwrap_or_else(|e| e.into_inner());
    mounts.retain(|m| m.root != root);
    mounts.push(Mount { root: root.clone(), host });
    ANY.store(true, Ordering::Release);
    root
}

pub fn unmount(root: &Path) {
    let mut mounts = MOUNTS.write().unwrap_or_else(|e| e.into_inner());
    mounts.retain(|m| m.root != root);
    ANY.store(!mounts.is_empty(), Ordering::Release);
}

/// The host a path is on and the path there, if it's under a mount.
pub fn remote(path: &Path) -> Option<(Arc<dyn Host>, String)> {
    if !ANY.load(Ordering::Acquire) {
        return None;
    }
    let mounts = MOUNTS.read().unwrap_or_else(|e| e.into_inner());
    let mount = mounts.iter().find(|m| path.starts_with(&m.root))?;
    let rest = path.strip_prefix(&mount.root).ok()?;
    Some((mount.host.clone(), local_to_host(rest)))
}

/// Whether a path is on a remote host.
pub fn is_remote(path: &Path) -> bool {
    remote(path).is_some()
}

/// The mount a path is under, if any.
pub fn mount_of(path: &Path) -> Option<PathBuf> {
    if !ANY.load(Ordering::Acquire) {
        return None;
    }
    let mounts = MOUNTS.read().unwrap_or_else(|e| e.into_inner());
    mounts.iter().find(|m| path.starts_with(&m.root)).map(|m| m.root.clone())
}

/// `rest` (relative to a mount) as the host's absolute path.
pub fn local_to_host(rest: &Path) -> String {
    let parts: Vec<String> = rest
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            _ => None,
        })
        .collect();
    format!("/{}", parts.join("/"))
}

/// A host's absolute path as the local path under `mount`.
pub fn local_path(mount: &Path, host_path: &str) -> PathBuf {
    let mut out = mount.to_path_buf();
    for part in host_path.split('/').filter(|p| !p.is_empty()) {
        out.push(part);
    }
    out
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the remote host isn't connected")
}

fn checked(host: &Arc<dyn Host>) -> io::Result<()> {
    if host.connected() { Ok(()) } else { Err(not_connected()) }
}

// ---- std::fs's functions --------------------------------------------------------

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.read(&p)),
        None => std::fs::read(path),
    }
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    match remote(path.as_ref()) {
        Some(_) => String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        None => std::fs::read_to_string(path),
    }
}

pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.write(&p, data.as_ref())),
        None => std::fs::write(path, data),
    }
}

/// Creates a file that mustn't exist yet, empty.
pub fn create_new(path: impl AsRef<Path>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.create(&p, &[], true)),
        None => std::fs::OpenOptions::new().write(true).create_new(true).open(path).map(|_| ()),
    }
}

/// A file to read from the start (a remote one is read whole first).
pub fn open(path: impl AsRef<Path>) -> io::Result<Box<dyn Read + Send>> {
    match remote(path.as_ref()) {
        Some(_) => Ok(Box::new(io::Cursor::new(read(path)?))),
        None => Ok(Box::new(std::fs::File::open(path)?)),
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.metadata(&p, true)).map(Metadata::Remote),
        None => std::fs::metadata(path).map(Metadata::Local),
    }
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.metadata(&p, false)).map(Metadata::Remote),
        None => std::fs::symlink_metadata(path).map(Metadata::Local),
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref();
    match remote(path) {
        Some((host, p)) => {
            checked(&host)?;
            let entries = host.read_dir(&p)?;
            Ok(ReadDir::Remote(path.to_path_buf(), entries.into_iter()))
        }
        None => std::fs::read_dir(path).map(ReadDir::Local),
    }
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.create_dir(&p, false)),
        None => std::fs::create_dir(path),
    }
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.create_dir(&p, true)),
        None => std::fs::create_dir_all(path),
    }
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.remove_file(&p)),
        None => std::fs::remove_file(path),
    }
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.remove_dir(&p, true)),
        None => std::fs::remove_dir_all(path),
    }
}

/// Renames within one host (a remote and a local path can't be renamed into each other).
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    match (remote(from.as_ref()), remote(to.as_ref())) {
        (None, None) => std::fs::rename(from, to),
        (Some((host, a)), Some((_, b))) => checked(&host).and_then(|_| host.rename(&a, &b)),
        _ => Err(io::Error::new(io::ErrorKind::CrossesDevices, "between a remote host and this computer")),
    }
}

/// Copies a file; across hosts too (read here, written there).
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    match (remote(from.as_ref()), remote(to.as_ref())) {
        (None, None) => std::fs::copy(from, to),
        (Some((host, a)), Some((other, b))) if Arc::ptr_eq(&host, &other) => checked(&host).and_then(|_| host.copy(&a, &b)),
        _ => {
            let data = read(from)?;
            write(to, &data)?;
            Ok(data.len() as u64)
        }
    }
}

/// The path with links and `..` resolved; a remote one stays under its mount.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match remote(path) {
        Some((host, p)) => {
            checked(&host)?;
            let mount = mount_of(path).ok_or_else(not_connected)?;
            Ok(local_path(&mount, &host.canonicalize(&p)?))
        }
        None => std::fs::canonicalize(path),
    }
}

pub fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    match remote(path.as_ref()) {
        Some((host, p)) => checked(&host).and_then(|_| host.read_link(&p)).map(PathBuf::from),
        None => std::fs::read_link(path),
    }
}

/// Moves a remote path to its host's trash, giving where it went (under the
/// same mount, so `rename` puts it back); None for a local one (the caller's
/// own trash code handles those).
pub fn trash_remote(path: &Path) -> Option<io::Result<PathBuf>> {
    let (host, p) = remote(path)?;
    let mount = mount_of(path)?;
    Some(checked(&host).and_then(|_| host.trash(&p)).map(|location| local_path(&mount, &location)))
}

/// `Path::is_file` & co, for any host.
pub trait PathExt {
    fn is_file_on_host(&self) -> bool;
    fn is_dir_on_host(&self) -> bool;
    fn exists_on_host(&self) -> bool;
}

impl PathExt for Path {
    fn is_file_on_host(&self) -> bool {
        match remote(self) {
            Some(_) => metadata(self).is_ok_and(|m| m.is_file()),
            None => self.is_file(),
        }
    }

    fn is_dir_on_host(&self) -> bool {
        match remote(self) {
            Some(_) => metadata(self).is_ok_and(|m| m.is_dir()),
            None => self.is_dir(),
        }
    }

    fn exists_on_host(&self) -> bool {
        match remote(self) {
            Some(_) => metadata(self).is_ok(),
            None => self.exists(),
        }
    }
}

// ---- std::fs's types --------------------------------------------------------------

/// `std::fs::Metadata`, or what a host said.
#[derive(Clone, Debug)]
pub enum Metadata {
    Local(std::fs::Metadata),
    Remote(HostMetadata),
}

impl Metadata {
    pub fn is_dir(&self) -> bool {
        match self {
            Metadata::Local(m) => m.is_dir(),
            Metadata::Remote(m) => m.is_dir,
        }
    }

    pub fn is_file(&self) -> bool {
        match self {
            Metadata::Local(m) => m.is_file(),
            Metadata::Remote(m) => m.is_file,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self {
            Metadata::Local(m) => m.is_symlink(),
            Metadata::Remote(m) => m.is_symlink,
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            Metadata::Local(m) => m.len(),
            Metadata::Remote(m) => m.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        match self {
            Metadata::Local(m) => m.modified(),
            Metadata::Remote(m) => m
                .modified_ms
                .map(|ms| SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(ms))
                .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no modification time")),
        }
    }

    pub fn readonly(&self) -> bool {
        match self {
            Metadata::Local(m) => m.permissions().readonly(),
            Metadata::Remote(m) => m.readonly,
        }
    }

    /// Unix: any execute bit is set (Git's file mode 100755).
    pub fn executable(&self) -> bool {
        match self {
            #[cfg(unix)]
            Metadata::Local(m) => std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0,
            #[cfg(not(unix))]
            Metadata::Local(_) => false,
            Metadata::Remote(m) => m.executable,
        }
    }

    /// What a host is told about it (`kuana-server` answering `fs/metadata`).
    pub fn to_host(&self) -> HostMetadata {
        match self {
            Metadata::Remote(m) => m.clone(),
            Metadata::Local(m) => HostMetadata {
                is_dir: m.is_dir(),
                is_file: m.is_file(),
                is_symlink: m.is_symlink(),
                len: m.len(),
                modified_ms: m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64),
                readonly: m.permissions().readonly(),
                executable: self.executable(),
            },
        }
    }
}

pub enum ReadDir {
    Local(std::fs::ReadDir),
    Remote(PathBuf, std::vec::IntoIter<HostEntry>),
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ReadDir::Local(dir) => dir.next().map(|e| e.map(DirEntry::Local)),
            ReadDir::Remote(parent, entries) => entries.next().map(|e| Ok(DirEntry::Remote(parent.join(&e.name), e))),
        }
    }
}

pub enum DirEntry {
    Local(std::fs::DirEntry),
    Remote(PathBuf, HostEntry),
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        match self {
            DirEntry::Local(e) => e.path(),
            DirEntry::Remote(path, _) => path.clone(),
        }
    }

    pub fn file_name(&self) -> std::ffi::OsString {
        match self {
            DirEntry::Local(e) => e.file_name(),
            DirEntry::Remote(_, e) => e.name.clone().into(),
        }
    }

    /// The entry's own type (a symlink is a symlink).
    pub fn file_type(&self) -> io::Result<FileType> {
        match self {
            DirEntry::Local(e) => e.file_type().map(|t| FileType { is_dir: t.is_dir(), is_file: t.is_file(), is_symlink: t.is_symlink() }),
            DirEntry::Remote(_, e) => Ok(FileType { is_dir: e.metadata.is_dir, is_file: e.metadata.is_file, is_symlink: e.metadata.is_symlink }),
        }
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        match self {
            DirEntry::Local(e) => e.metadata().map(Metadata::Local),
            DirEntry::Remote(_, e) => Ok(Metadata::Remote(e.metadata.clone())),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        self.is_file
    }

    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;

    /// A host keeping files in memory.
    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, Vec<u8>>>);

    impl Host for Memory {
        fn read(&self, path: &str) -> io::Result<Vec<u8>> {
            self.0.lock().unwrap().get(path).cloned().ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().insert(path.to_string(), data.to_vec());
            Ok(())
        }
        fn create(&self, path: &str, data: &[u8], create_new: bool) -> io::Result<()> {
            if create_new && self.0.lock().unwrap().contains_key(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            self.write(path, data)
        }
        fn metadata(&self, path: &str, _: bool) -> io::Result<HostMetadata> {
            let files = self.0.lock().unwrap();
            if let Some(data) = files.get(path) {
                return Ok(HostMetadata { is_file: true, len: data.len() as u64, ..Default::default() });
            }
            let dir = format!("{}/", path.trim_end_matches('/'));
            if files.keys().any(|k| k.starts_with(&dir)) {
                return Ok(HostMetadata { is_dir: true, ..Default::default() });
            }
            Err(io::ErrorKind::NotFound.into())
        }
        fn read_dir(&self, path: &str) -> io::Result<Vec<HostEntry>> {
            let dir = format!("{}/", path.trim_end_matches('/'));
            let files = self.0.lock().unwrap();
            let mut names: Vec<String> = files.keys().filter_map(|k| k.strip_prefix(&dir)).map(|rest| rest.split('/').next().unwrap().to_string()).collect();
            names.sort();
            names.dedup();
            Ok(names.into_iter().map(|name| HostEntry { metadata: HostMetadata { is_file: files.contains_key(&format!("{dir}{name}")), ..Default::default() }, name }).collect())
        }
        fn create_dir(&self, _: &str, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn remove_file(&self, path: &str) -> io::Result<()> {
            self.0.lock().unwrap().remove(path).map(|_| ()).ok_or_else(|| io::ErrorKind::NotFound.into())
        }
        fn remove_dir(&self, _: &str, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            let data = self.read(from)?;
            self.remove_file(from)?;
            self.write(to, &data)
        }
        fn copy(&self, from: &str, to: &str) -> io::Result<u64> {
            let data = self.read(from)?;
            self.write(to, &data)?;
            Ok(data.len() as u64)
        }
        fn canonicalize(&self, path: &str) -> io::Result<String> {
            Ok(path.to_string())
        }
        fn read_link(&self, _: &str) -> io::Result<String> {
            Err(io::ErrorKind::InvalidInput.into())
        }
        fn trash(&self, path: &str) -> io::Result<String> {
            let to = format!("/trash{path}");
            self.rename(path, &to)?;
            Ok(to)
        }
    }

    #[test]
    fn mounted_paths_go_to_their_host_and_others_stay_local() {
        let root = mount("fs-test-host", Arc::new(Memory::default()));
        let project = root.join("home/me/project");
        write(project.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(read_to_string(project.join("src/main.rs")).unwrap(), "fn main() {}");
        assert!(project.join("src/main.rs").is_file_on_host());
        assert!(project.join("src").is_dir_on_host());
        assert!(!project.join("nope").exists_on_host());
        let names: Vec<String> = read_dir(&project).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names, ["src"]);
        let entry = read_dir(project.join("src")).unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path(), project.join("src/main.rs"));
        assert!(entry.file_type().unwrap().is_file());
        // The host sees its own paths.
        let (_, host) = remote(&project.join("src/main.rs")).unwrap();
        assert_eq!(host, "/home/me/project/src/main.rs");
        assert_eq!(canonicalize(project.join("src/main.rs")).unwrap(), project.join("src/main.rs"));
        // Renames stay on their host; copies cross.
        rename(project.join("src/main.rs"), project.join("src/lib.rs")).unwrap();
        assert!(create_new(project.join("src/lib.rs")).is_err());
        let local = std::env::temp_dir().join(format!("kuana-fs-{}.rs", std::process::id()));
        copy(project.join("src/lib.rs"), &local).unwrap();
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "fn main() {}");
        assert!(rename(&local, project.join("x.rs")).is_err());
        // Local paths go to std::fs as before.
        assert!(local.is_file_on_host());
        std::fs::remove_file(&local).unwrap();
        unmount(&root);
        assert!(read(project.join("src/lib.rs")).is_err() || !is_remote(&project));
    }

    #[test]
    fn host_and_local_paths_convert() {
        let mount = mount_root("box");
        assert_eq!(local_path(&mount, "/home/me/a b.rs"), mount.join("home").join("me").join("a b.rs"));
        assert_eq!(local_to_host(Path::new("home/me/x")), "/home/me/x");
        assert_eq!(local_to_host(Path::new("")), "/");
        assert!(mount_root("user@box:22").to_string_lossy().contains("user@box_22"));
    }
}
