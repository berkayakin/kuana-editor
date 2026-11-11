//! What the user allowed extensions' code to do (`[kuana] permissions`):
//! `<config>/extensions/permissions.json`, `{"<extension id>": ["process", …]}`.
//! Kuana asks the first time the code needs a permission (`ui_host`); an
//! answer of no lasts until Kuana restarts, so declining isn't permanent and
//! isn't asked again and again either.

use std::{
    collections::{BTreeMap, HashSet},
    sync::{LazyLock, Mutex},
};

use super::contrib::Permission;

type Granted = BTreeMap<String, Vec<String>>;

static GRANTED: LazyLock<Mutex<Granted>> = LazyLock::new(|| Mutex::new(if cfg!(test) { Granted::new() } else { load() }));
static DECLINED: LazyLock<Mutex<HashSet<(String, Permission)>>> = LazyLock::new(Default::default);

fn file() -> Option<std::path::PathBuf> {
    super::extensions_dir().map(|d| d.join("permissions.json"))
}

fn load() -> Granted {
    let Some(path) = file() else { return Granted::new() };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
            crate::state::set_aside(&path, "extension permissions", err);
            Granted::new()
        }),
        Err(_) => Granted::new(),
    }
}

fn save(granted: &Granted) {
    // Tests keep their answers in memory, never in the user's config.
    if cfg!(test) {
        return;
    }
    let (Some(path), Ok(json)) = (file(), serde_json::to_vec_pretty(granted)) else { return };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    crate::state::write_atomic(&path, &json).ok();
}

pub fn is_granted(extension: &str, permission: Permission) -> bool {
    GRANTED.lock().unwrap_or_else(|e| e.into_inner()).get(extension).is_some_and(|keys| keys.iter().any(|k| k == permission.key()))
}

pub fn was_declined(extension: &str, permission: Permission) -> bool {
    DECLINED.lock().unwrap_or_else(|e| e.into_inner()).contains(&(extension.to_string(), permission))
}

/// Records the user's answer.
pub fn answer(extension: &str, permission: Permission, allowed: bool) {
    if !allowed {
        DECLINED.lock().unwrap_or_else(|e| e.into_inner()).insert((extension.to_string(), permission));
        return;
    }
    let mut granted = GRANTED.lock().unwrap_or_else(|e| e.into_inner());
    let keys = granted.entry(extension.to_string()).or_default();
    if !keys.iter().any(|k| k == permission.key()) {
        keys.push(permission.key().to_string());
        save(&granted);
    }
}

/// The permissions an extension was given (for its row in Preferences).
pub fn granted(extension: &str) -> Vec<Permission> {
    let granted = GRANTED.lock().unwrap_or_else(|e| e.into_inner());
    granted.get(extension).into_iter().flatten().filter_map(|k| Permission::from_key(k)).collect()
}

/// Takes back everything an extension was given (uninstalling, or the
/// user revoking it).
pub fn revoke(extension: &str) {
    let mut granted = GRANTED.lock().unwrap_or_else(|e| e.into_inner());
    if granted.remove(extension).is_some() {
        save(&granted);
    }
    DECLINED.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _)| id != extension);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_kept_and_revoked() {
        let id = "test-permissions-ext";
        assert!(!is_granted(id, Permission::Process));
        answer(id, Permission::Network, false);
        assert!(was_declined(id, Permission::Network) && !is_granted(id, Permission::Network));
        answer(id, Permission::Process, true);
        answer(id, Permission::Process, true);
        assert!(is_granted(id, Permission::Process));
        assert_eq!(granted(id), [Permission::Process]);
        revoke(id);
        assert!(!is_granted(id, Permission::Process) && !was_declined(id, Permission::Network));
    }
}
