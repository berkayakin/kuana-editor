//! Files kept away from AI: `.aiignore` and `.cursorignore` files (gitignore
//! syntax) in a file's folder or any folder above it name what Kuana's agent
//! may not list, search, read or change and what tab completion doesn't send.
//! Each ignore file is read once and again only when it changes.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The ignore files read, by name, in each folder.
pub const FILES: [&str; 2] = [".aiignore", ".cursorignore"];

/// How long a folder's ignore files aren't looked at again (listing a
/// project asks for every file, folder by folder).
const FRESH_FOR: Duration = Duration::from_secs(1);

/// A folder's rules (None: it has no ignore file), with the modification
/// times they were read at and when those were last looked at.
struct Rules {
    stamps: Vec<Option<SystemTime>>,
    checked: Option<Instant>,
    matcher: Option<Gitignore>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Rules>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Rules>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Whether `path` is kept away from AI by an ignore file in its folder or
/// one above it (up to the file system's root).
pub fn excluded(path: &Path) -> bool {
    let is_dir = path.is_dir();
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    path.ancestors().skip(1).any(|folder| {
        let rules = cache.entry(folder.to_path_buf()).or_insert_with(|| Rules { stamps: Vec::new(), checked: None, matcher: None });
        if rules.checked.is_none_or(|at| at.elapsed() >= FRESH_FOR) {
            rules.checked = Some(Instant::now());
            let stamps: Vec<Option<SystemTime>> = FILES.iter().map(|name| std::fs::metadata(folder.join(name)).and_then(|m| m.modified()).ok()).collect();
            if rules.stamps != stamps {
                rules.matcher = stamps.iter().any(Option::is_some).then(|| {
                    let mut builder = GitignoreBuilder::new(folder);
                    for name in FILES {
                        // A rule that doesn't parse is skipped, as Git does.
                        let _ = builder.add(folder.join(name));
                    }
                    builder.build().ok()
                }).flatten();
                rules.stamps = stamps;
            }
        }
        rules.matcher.as_ref().is_some_and(|m| m.matched_path_or_any_parents(path, is_dir).is_ignore())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ignore_files_keep_paths_away() {
        let root = std::env::temp_dir().join(format!("kuana-aiignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("secrets/deep")).unwrap();
        std::fs::create_dir_all(root.join("src/gen")).unwrap();
        std::fs::write(root.join(".aiignore"), "secrets/\n*.key\n").unwrap();
        std::fs::write(root.join("src/.cursorignore"), "gen/\n!gen/keep.rs\n").unwrap();
        for file in ["secrets/deep/a.txt", "id.key", "src/main.rs", "src/gen/out.rs", "src/gen/keep.rs"] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        let excluded = |rel: &str| super::excluded(&root.join(rel));
        assert!(excluded("secrets/deep/a.txt") && excluded("id.key") && excluded("src/gen/out.rs"));
        assert!(!excluded("src/main.rs"));
        // A change to the file is seen (once its folder is looked at again).
        std::thread::sleep(super::FRESH_FOR);
        std::fs::write(root.join(".aiignore"), "*.rs\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options().write(true).open(root.join(".aiignore")).unwrap().set_modified(later).unwrap();
        assert!(excluded("src/main.rs") && !excluded("secrets/deep/a.txt"));
        std::fs::remove_dir_all(&root).ok();
    }
}
