//! Files that hold a secret (a token an agent must send back): written
//! whole and readable only by the user, in folders only the user can enter,
//! and the check that the process that wrote one still runs. Used by the
//! app's agent connections (`agent_ide.rs`) and by `kuana-server`, which
//! writes Claude Code's lock file on a remote host (`claude_lock`).

use std::{
    fs,
    io::{self, Write as _},
    path::Path,
};

/// Makes a folder for files holding a token, readable only by the user.
/// `strict` (Gemini CLI checks it): an existing one must be a real folder
/// the user owns, and is narrowed to 0700.
pub fn private_dir(dir: &Path, strict: bool) -> io::Result<()> {
    if !dir.is_dir() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    #[cfg(unix)]
    if strict {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let meta = fs::symlink_metadata(dir)?;
        // SAFETY: getuid has no preconditions.
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            return Err(io::Error::other(format!("{} isn't a folder of this user's", dir.display())));
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = strict;
    Ok(())
}

/// Writes a file whole (a temp file beside it, renamed over it), readable
/// only by the user: it holds a token.
pub fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no folder"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temp).and_then(|mut file| {
        file.write_all(content)?;
        file.sync_all()
    });
    match written.and_then(|()| fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Whether a process with this id runs (a Kuana that left its files behind
/// after a crash doesn't).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // Not a real process id (0 and negative ones name process groups).
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    let tasklist = std::env::var_os("SystemRoot").map(|root| Path::new(&root).join("System32").join("tasklist.exe")).filter(|p| p.is_file());
    let output = crate::process::command(tasklist.as_deref().unwrap_or("tasklist".as_ref()))
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .output();
    // Can't tell: its file stays.
    output.map_or(true, |out| tasklist_lists(&String::from_utf8_lossy(&out.stdout), pid))
}

/// Whether `tasklist /FO CSV /NH` printed a row for `pid` (`"name","pid",…`);
/// without one it prints a message in the system's language.
#[cfg(any(windows, test))]
fn tasklist_lists(output: &str, pid: u32) -> bool {
    let column = format!("\",\"{pid}\",\"");
    output.lines().any(|line| line.contains(&column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_files_are_the_users_alone() {
        let dir = std::env::temp_dir().join(format!("kuana-agent-ide-{}", std::process::id())).join("ide");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        private_dir(&dir, true).unwrap();
        let file = dir.join("a.json");
        write_private(&file, b"{}").unwrap();
        write_private(&file, b"{\"x\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"x\":1}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            // A strict folder left open to others is narrowed.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            private_dir(&dir, true).unwrap();
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            // A link isn't taken for the folder.
            let link = dir.parent().unwrap().join("link");
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert!(private_dir(&link, true).is_err());
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp file left");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn reads_tasklists_rows() {
        let listed = "\"kuana.exe\",\"4242\",\"Console\",\"1\",\"123,456 K\"\r\n";
        assert!(tasklist_lists(listed, 4242));
        assert!(!tasklist_lists(listed, 42));
        assert!(!tasklist_lists("INFO: No tasks are running which match the specified criteria.\r\n", 4242));
    }
}
