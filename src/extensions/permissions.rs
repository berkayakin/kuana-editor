//! What the user allowed extensions' code to do (`[kuana] permissions`):
//! `<config>/extensions/permissions.json`, `{"<extension id>": ["process", …]}`.
//! A permission the manifest narrows (`[kuana.allow]`) is kept per item,
//! `"process:docker"`, `"network:api.github.com"`; a bare key is a grant of
//! anything it covers. So an update that widens the list, or drops it, asks
//! again for what wasn't allowed. Kuana asks the first time the code needs a
//! permission (`ui_host`); an answer of no lasts until Kuana restarts, so
//! declining isn't permanent and isn't asked again and again either.
//!
//! What a list allows: `allows_program`, `allows_host`, `plain_command`.

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

/// Whether the user allowed `permission` for `scope`, the manifest's list
/// (None: anything it covers).
pub fn is_granted(extension: &str, permission: Permission, scope: Option<&[String]>) -> bool {
    let granted = GRANTED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(keys) = granted.get(extension) else { return false };
    covers(keys, permission, scope)
}

fn covers(keys: &[String], permission: Permission, scope: Option<&[String]>) -> bool {
    let has = |key: &str| keys.iter().any(|k| k == key);
    has(permission.key()) || scope.is_some_and(|items| !items.is_empty() && items.iter().all(|item| has(&item_key(permission, item))))
}

fn item_key(permission: Permission, item: &str) -> String {
    format!("{}:{item}", permission.key())
}

pub fn was_declined(extension: &str, permission: Permission) -> bool {
    DECLINED.lock().unwrap_or_else(|e| e.into_inner()).contains(&(extension.to_string(), permission))
}

/// Records the user's answer, for `scope` as the prompt said it.
pub fn answer(extension: &str, permission: Permission, scope: Option<&[String]>, allowed: bool) {
    if !allowed {
        DECLINED.lock().unwrap_or_else(|e| e.into_inner()).insert((extension.to_string(), permission));
        return;
    }
    let mut granted = GRANTED.lock().unwrap_or_else(|e| e.into_inner());
    let keys = granted.entry(extension.to_string()).or_default();
    let new: Vec<String> = match scope {
        None => vec![permission.key().to_string()],
        Some(items) => items.iter().map(|item| item_key(permission, item)).collect(),
    };
    let before = keys.len();
    for key in new {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.len() != before {
        save(&granted);
    }
}

/// Whether the extension was given anything (for its row in Preferences).
pub fn any_granted(extension: &str) -> bool {
    GRANTED.lock().unwrap_or_else(|e| e.into_inner()).get(extension).is_some_and(|keys| !keys.is_empty())
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

/// Whether `program`, as the code names it, is one `allowed` lists: the
/// bare name (on Windows also with `.exe`, in any case), never a path.
pub fn allows_program(allowed: &[String], program: &str) -> bool {
    if program.contains(['/', '\\']) {
        return false;
    }
    if cfg!(windows) {
        let lower = program.to_ascii_lowercase();
        let name = lower.strip_suffix(".exe").unwrap_or(&lower);
        allowed.iter().any(|a| a.eq_ignore_ascii_case(name))
    } else {
        allowed.iter().any(|a| a == program)
    }
}

/// Whether `host` (lowercase, as `url` gives it) is one `allowed` lists:
/// exactly, or under a `*.name` entry (`*.example.com` takes
/// `api.example.com` and `a.b.example.com`, not `example.com`).
pub fn allows_host(allowed: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.');
    allowed.iter().any(|entry| match entry.strip_prefix("*.") {
        Some(base) => host.strip_suffix(base).is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        None => entry == host,
    })
}

/// The program a command line for `run-in-terminal` starts, when it's a
/// plain one: a first word naming the program, then words of letters,
/// digits and `_-./:,+=%`, single-quoted text or double-quoted text of those
/// and spaces. No pipes, redirections, variables, globs or second commands,
/// so the line starts that program and nothing else, read the same by a
/// POSIX shell and PowerShell (which a Run tab uses on Windows).
pub fn plain_command(command: &str) -> Option<&str> {
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | ',' | '+' | '=' | '%');
    let command = command.trim();
    let first_end = command.find(' ').unwrap_or(command.len());
    let program = &command[..first_end];
    if program.is_empty() || !program.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')) {
        return None;
    }
    let mut chars = command[first_end..].chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' => {}
            '\'' => {
                // Literal to the closing quote in both shells.
                if !chars.by_ref().any(|c| c == '\'') {
                    return None;
                }
            }
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) if safe(c) || c == ' ' => {}
                    _ => return None,
                }
            },
            c if safe(c) => {}
            _ => return None,
        }
    }
    Some(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn answers_are_kept_and_revoked() {
        let id = "test-permissions-ext";
        assert!(!is_granted(id, Permission::Process, None));
        answer(id, Permission::Network, None, false);
        assert!(was_declined(id, Permission::Network) && !is_granted(id, Permission::Network, None));
        answer(id, Permission::Process, None, true);
        answer(id, Permission::Process, None, true);
        assert!(is_granted(id, Permission::Process, None));
        assert!(any_granted(id));
        revoke(id);
        assert!(!is_granted(id, Permission::Process, None) && !was_declined(id, Permission::Network));
        assert!(!any_granted(id));
    }

    #[test]
    fn a_wider_list_asks_again() {
        let id = "test-permissions-widen";
        let docker = list(&["docker"]);
        answer(id, Permission::Process, Some(&docker), true);
        assert!(is_granted(id, Permission::Process, Some(&docker)));
        // An update adds a program, or drops the list: neither is covered.
        assert!(!is_granted(id, Permission::Process, Some(&list(&["docker", "git"]))));
        assert!(!is_granted(id, Permission::Process, None));
        // Another permission's items don't count.
        assert!(!is_granted(id, Permission::Network, Some(&docker)));
        answer(id, Permission::Process, Some(&list(&["docker", "git"])), true);
        assert!(is_granted(id, Permission::Process, Some(&list(&["git"]))));
        // An empty list never counts as covered.
        assert!(!is_granted(id, Permission::Process, Some(&[])));
        // Anything (a grant from before lists, or of an unrestricted one) covers a list.
        answer(id, Permission::Network, None, true);
        assert!(is_granted(id, Permission::Network, Some(&list(&["api.github.com"]))));
        revoke(id);
    }

    #[test]
    fn programs_match_by_bare_name() {
        let allowed = list(&["docker", "git"]);
        assert!(allows_program(&allowed, "docker"));
        assert!(allows_program(&allowed, "git"));
        assert!(!allows_program(&allowed, "sh"));
        assert!(!allows_program(&allowed, "/tmp/docker"));
        assert!(!allows_program(&allowed, "bin/docker"));
        assert!(!allows_program(&allowed, "..\\docker"));
        assert!(!allows_program(&allowed, "dockerd"));
        assert_eq!(allows_program(&allowed, "docker.exe"), cfg!(windows));
    }

    #[test]
    fn hosts_match_exactly_or_under_a_wildcard() {
        let allowed = list(&["api.github.com", "*.example.com"]);
        assert!(allows_host(&allowed, "api.github.com"));
        assert!(allows_host(&allowed, "api.github.com."));
        assert!(!allows_host(&allowed, "github.com"));
        assert!(!allows_host(&allowed, "evilapi.github.com"));
        assert!(allows_host(&allowed, "a.example.com"));
        assert!(allows_host(&allowed, "a.b.example.com"));
        assert!(!allows_host(&allowed, "example.com"));
        assert!(!allows_host(&allowed, "badexample.com"));
        assert!(!allows_host(&allowed, "example.com.evil.net"));
    }

    #[test]
    fn only_plain_command_lines_name_their_program() {
        assert_eq!(plain_command("docker compose -f '/a b/compose.yaml' up -d"), Some("docker"));
        assert_eq!(plain_command("git log --format=%h -n 3"), Some("git"));
        assert_eq!(plain_command("docker run \"my image\""), Some("docker"));
        for line in [
            "docker ps; rm -rf ~",
            "docker ps && curl x",
            "docker ps | sh",
            "docker ps > out",
            "docker $(id)",
            "docker `id`",
            "docker \"$HOME\"",
            "docker 'open",
            "docker ps\nrm x",
            "/usr/bin/docker ps",
            "FOO=1 docker ps",
            "docker *",
            "",
        ] {
            assert_eq!(plain_command(line), None, "{line}");
        }
    }
}
