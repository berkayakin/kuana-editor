//! Claude Code's IDE lock files (its `/ide` command): an IDE listening for
//! Claude on a loopback port says so in `<dir>/<port>.lock`, where `<dir>` is
//! `$CLAUDE_CONFIG_DIR/ide`, else `~/.claude/ide`. Kuana writes one for each
//! local project window (`claude_ide.rs` in the app) and `kuana-server` one on
//! a remote host for a window whose project is there (`remote::server`'s
//! `listen/start`), so both write them here.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::private_files;

/// How Claude Code lists Kuana in `/ide`.
pub const IDE_NAME: &str = "Kuana";

/// Where Claude Code looks for IDEs' lock files, from `CLAUDE_CONFIG_DIR`
/// and the home folder.
pub fn lock_dir(config_dir: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    match config_dir.filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join("ide")),
        None => home.map(|home| home.join(".claude").join("ide")),
    }
}

/// Whether Claude Code has run here (the lock folder's parent exists), so
/// people without it don't get a folder made in their home.
pub fn installed(dir: &Path) -> bool {
    dir.parent().is_some_and(Path::is_dir)
}

/// Writes `<dir>/<port>.lock` whole, readable only by the user: it holds
/// the token. `pid` is the process Claude may check is still there.
pub fn write(dir: &Path, port: u16, pid: u32, folders: &[String], token: &str) -> io::Result<PathBuf> {
    let content = json!({
        "pid": pid,
        "workspaceFolders": folders,
        "ideName": IDE_NAME,
        "transport": "ws",
        "authToken": token,
    });
    private_files::private_dir(dir, false)?;
    let path = dir.join(format!("{port}.lock"));
    private_files::write_private(&path, content.to_string().as_bytes())?;
    Ok(path)
}

/// Removes the lock files of Kuanas (or `kuana-server`s) that ended without
/// removing theirs (a crash, a kill), so `/ide` doesn't offer them.
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "lock") {
            continue;
        }
        let Some(lock) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else { continue };
        let Some(pid) = lock.get("pid").and_then(Value::as_u64) else { continue };
        if lock.get("ideName").and_then(Value::as_str) == Some(IDE_NAME) && pid != u64::from(std::process::id()) && !private_files::alive(pid as u32) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuana-claude-lock-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_folder_follows_claudes_config_dir() {
        assert_eq!(lock_dir(Some("/c/cfg"), Some(Path::new("/home/me"))), Some(PathBuf::from("/c/cfg/ide")));
        assert_eq!(lock_dir(Some(""), Some(Path::new("/home/me"))), Some(PathBuf::from("/home/me/.claude/ide")));
        assert_eq!(lock_dir(None, None), None);
    }

    #[test]
    fn writes_a_private_lock_file() {
        let dir = temp_dir("write");
        let path = write(&dir, 4242, 7, &["/work/app".into()], "secret").unwrap();
        assert_eq!(path, dir.join("4242.lock"));
        let lock: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(lock, json!({ "pid": 7, "workspaceFolders": ["/work/app"], "ideName": "Kuana", "transport": "ws", "authToken": "secret" }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // Rewritten in place, nothing left beside it.
        write(&dir, 4242, 7, &["/work/app".into(), "/work/lib".into()], "secret").unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removes_lock_files_of_kuanas_that_are_gone() {
        let dir = temp_dir("stale");
        fs::create_dir_all(&dir).unwrap();
        // A pid no process has (above the usual limits).
        let gone = json!({ "pid": 2_147_483_000u32, "ideName": "Kuana" });
        let other_ide = json!({ "pid": 2_147_483_000u32, "ideName": "Visual Studio Code" });
        let this_one = json!({ "pid": std::process::id(), "ideName": "Kuana" });
        fs::write(dir.join("1.lock"), gone.to_string()).unwrap();
        fs::write(dir.join("2.lock"), other_ide.to_string()).unwrap();
        fs::write(dir.join("3.lock"), this_one.to_string()).unwrap();
        remove_stale(&dir);
        assert_eq!(dir.join("1.lock").exists(), !cfg!(unix));
        assert!(dir.join("2.lock").exists() && dir.join("3.lock").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
