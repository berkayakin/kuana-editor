//! Paths as Kuana keeps them: canonical, and on Windows without the `\\?\`
//! prefix `std::fs::canonicalize` adds. Git, shells and most programs don't
//! take that form (`git init \\?\C:\…` fails), and it never equals the same
//! path written without it, so a path canonicalized one way and one taken
//! from a dialog, a file URI or Git wouldn't match.

use std::path::{Path, PathBuf};

/// `std::fs::canonicalize`, minus the `\\?\` prefix on Windows wherever the
/// path means the same without it (see `plain`).
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(path)?;
    if !cfg!(windows) {
        return Ok(real);
    }
    Ok(real.to_str().and_then(plain).map(PathBuf::from).unwrap_or(real))
}

/// A verbatim Windows path in its ordinary form: `\\?\C:\a` → `C:\a`,
/// `\\?\UNC\server\share\a` → `\\server\share\a`. None when it isn't one of
/// those, or when the ordinary form wouldn't mean the same: past the classic
/// length limit, or with a component Windows reads otherwise without the
/// prefix (a device name like `nul`, a trailing dot or space, a `/`).
fn plain(path: &str) -> Option<String> {
    let (lead, rest, first) = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        (r"\\", rest, 0)
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        // A drive letter, a colon and its root (`C:` alone is relative: the
        // current folder on that drive).
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
        if !drive {
            return None;
        }
        ("", rest, 1)
    } else {
        return None;
    };
    // MAX_PATH, its terminating NUL included.
    if lead.len() + rest.len() >= 260 {
        return None;
    }
    let components: Vec<&str> = rest.split('\\').skip(first).collect();
    let last = components.len().saturating_sub(1);
    for (ix, component) in components.iter().enumerate() {
        // Only a root (or a share's) ends with a separator.
        if component.is_empty() {
            if ix == last {
                continue;
            }
            return None;
        }
        if component.contains('/') || component.ends_with(['.', ' ']) || is_device_name(component) {
            return None;
        }
    }
    Some(format!("{lead}{rest}"))
}

/// Whether Windows takes `component` for a device without the `\\?\`
/// prefix: `con`, `nul.txt`, `COM1` (any case, any extension).
fn is_device_name(component: &str) -> bool {
    let base = component.split('.').next().unwrap_or_default().trim_end_matches(' ');
    let numbered = |prefix: &str| {
        base.len() == 4 && base.get(..3).is_some_and(|p| p.eq_ignore_ascii_case(prefix)) && base.as_bytes()[3].is_ascii_digit()
    };
    ["con", "prn", "aux", "nul"].iter().any(|name| base.eq_ignore_ascii_case(name)) || numbered("com") || numbered("lpt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_lose_the_prefix_only_where_it_changes_nothing() {
        assert_eq!(plain(r"\\?\C:\Users\me\a.rs").as_deref(), Some(r"C:\Users\me\a.rs"));
        assert_eq!(plain(r"\\?\d:\").as_deref(), Some(r"d:\"));
        assert_eq!(plain(r"\\?\UNC\server\share\dir").as_deref(), Some(r"\\server\share\dir"));
        // Not verbatim, or not a drive or share.
        assert_eq!(plain(r"C:\a"), None);
        assert_eq!(plain("/home/me/a"), None);
        assert_eq!(plain(r"\\?\C:"), None);
        assert_eq!(plain(r"\\?\Volume{0123}\a"), None);
        assert_eq!(plain(r"\\?\CC:\a"), None);
        // Names that mean something else without the prefix.
        assert_eq!(plain(r"\\?\C:\dir\nul"), None);
        assert_eq!(plain(r"\\?\C:\dir\Con.txt"), None);
        assert_eq!(plain(r"\\?\C:\dir\com7"), None);
        assert_eq!(plain(r"\\?\C:\dir\a."), None);
        assert_eq!(plain(r"\\?\C:\dir\a \b"), None);
        assert_eq!(plain(r"\\?\C:\dir\a/b"), None);
        assert_eq!(plain(r"\\?\C:\dir\\b"), None);
        // Ordinary names that only look close.
        assert_eq!(plain(r"\\?\C:\dir\console\company\nul2").as_deref(), Some(r"C:\dir\console\company\nul2"));
        assert_eq!(plain(r"\\?\C:\dir\abé").as_deref(), Some(r"C:\dir\abé"));
        // Past the classic limit only the verbatim form works.
        let long = format!(r"\\?\C:\{}", "a".repeat(260));
        assert_eq!(plain(&long), None);
    }

    #[test]
    fn canonical_paths_are_absolute_and_real() {
        let dir = std::env::temp_dir();
        let real = canonicalize(&dir).unwrap();
        assert!(real.is_absolute() && real.is_dir());
        assert!(!real.to_string_lossy().starts_with(r"\\?\"), "{}", real.display());
    }
}
