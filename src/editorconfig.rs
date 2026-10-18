//! `.editorconfig` support (editorconfig.org): the indent and whitespace
//! properties, read from the file's folder up to the nearest `root = true`.

use std::path::Path;

use regex::Regex;

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct EditorConfig {
    /// `indent_style`: tab (true) or space (false).
    pub hard_tabs: Option<bool>,
    pub indent_size: Option<usize>,
    pub tab_width: Option<usize>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    /// Shown as a ruler.
    pub max_line_length: Option<usize>,
}

impl EditorConfig {
    /// The properties that apply to `path`. Missing or unreadable files are
    /// skipped; closer files and later sections win.
    pub fn for_file(path: &Path) -> EditorConfig {
        let mut files = Vec::new();
        for dir in path.ancestors().skip(1) {
            if let Ok(text) = std::fs::read_to_string(dir.join(".editorconfig")) {
                let root = is_root(&text);
                files.push((dir.to_path_buf(), text));
                if root {
                    break;
                }
            }
        }
        let mut config = EditorConfig::default();
        for (dir, text) in files.iter().rev() {
            if let Ok(rel) = path.strip_prefix(dir) {
                config.apply(text, &rel.to_string_lossy().replace('\\', "/"));
            }
        }
        // `indent_size = tab` means "as wide as a tab".
        if config.indent_size.is_none() && config.hard_tabs == Some(true) {
            config.indent_size = config.tab_width;
        }
        config
    }

    /// Applies the sections of one file that match `rel` (relative to it).
    fn apply(&mut self, text: &str, rel: &str) {
        let mut matches = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                matches = glob_matches(section, rel);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            if !matches {
                continue;
            }
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_ascii_lowercase());
            // "unset" clears a property set by an outer file.
            let unset = value == "unset";
            let flag = match value.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            match key.as_str() {
                "indent_style" => {
                    self.hard_tabs = match value.as_str() {
                        "tab" => Some(true),
                        "space" => Some(false),
                        _ if unset => None,
                        _ => self.hard_tabs,
                    }
                }
                "indent_size" => {
                    self.indent_size = if value == "tab" || unset { None } else { value.parse().ok().or(self.indent_size) }
                }
                "tab_width" => self.tab_width = if unset { None } else { value.parse().ok().or(self.tab_width) },
                "trim_trailing_whitespace" => {
                    self.trim_trailing_whitespace = if unset { None } else { flag.or(self.trim_trailing_whitespace) }
                }
                "insert_final_newline" => {
                    self.insert_final_newline = if unset { None } else { flag.or(self.insert_final_newline) }
                }
                "max_line_length" => {
                    self.max_line_length = if unset || value == "off" { None } else { value.parse().ok().or(self.max_line_length) }
                }
                _ => {}
            }
        }
    }
}

/// Whether the preamble (before any section) says `root = true`.
fn is_root(text: &str) -> bool {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return false;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("root")
        {
            return v.trim().eq_ignore_ascii_case("true");
        }
    }
    false
}

/// EditorConfig globs: `*` (not `/`), `**`, `?`, `[set]`, `[!set]`,
/// `{a,b}` and `{1..3}`. Patterns without `/` match the file name in any
/// folder; others are relative to the `.editorconfig`'s folder.
pub fn glob_matches(pattern: &str, rel: &str) -> bool {
    let anchored = pattern.contains('/');
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let mut re = String::from(if anchored { "^" } else { "^(?:.*/)?" });
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let mut braces = 0;
    let mut ranges = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => match chars[i..].iter().position(|&c| c == ']') {
                Some(len) => {
                    let set: String = chars[i + 1..i + len].iter().collect();
                    let set = set.strip_prefix('!').map_or_else(|| set.clone(), |s| format!("^{s}"));
                    re.push('[');
                    re.push_str(&set.replace('\\', "\\\\"));
                    re.push(']');
                    i += len;
                }
                None => re.push_str("\\["),
            },
            '{' => {
                let close = chars[i..].iter().position(|&c| c == '}').map(|p| i + p);
                let inner: Option<String> = close.map(|e| chars[i + 1..e].iter().collect());
                if let Some((a, b)) = inner.as_deref().and_then(|s| s.split_once("..")) {
                    // A numeric range: any integer, checked against the
                    // bounds after matching (spelling them all out would
                    // hang on `{1..100000000000}`).
                    if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                        re.push_str("([+-]?[0-9]+)");
                        ranges.push(a.min(b)..=a.max(b));
                        i = close.unwrap();
                    } else {
                        re.push_str("\\{");
                    }
                } else if inner.as_deref().is_some_and(|s| s.contains(',')) {
                    re.push_str("(?:");
                    braces += 1;
                } else {
                    re.push_str("\\{");
                }
            }
            ',' if braces > 0 => re.push('|'),
            '}' if braces > 0 => {
                re.push(')');
                braces -= 1;
            }
            '\\' if i + 1 < chars.len() => {
                re.push_str(&regex::escape(&chars[i + 1].to_string()));
                i += 1;
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    let Ok(re) = Regex::new(&re) else { return false };
    let Some(caps) = re.captures(rel) else { return false };
    // Group n + 1 is the nth range; one in an alternative not taken is absent.
    ranges.iter().enumerate().all(|(n, range)| {
        caps.get(n + 1).is_none_or(|m| m.as_str().parse::<i64>().is_ok_and(|v| range.contains(&v)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_matches("*", "a.rs"));
        assert!(glob_matches("*", "src/a.rs"));
        assert!(glob_matches("*.{js,ts}", "src/app.ts"));
        assert!(!glob_matches("*.{js,ts}", "src/app.rs"));
        assert!(glob_matches("Makefile", "sub/Makefile"));
        assert!(glob_matches("lib/**.js", "lib/a/b.js"));
        assert!(!glob_matches("lib/*.js", "lib/a/b.js"));
        assert!(!glob_matches("lib/*.js", "x/lib/b.js"));
        assert!(glob_matches("/docs/*.md", "docs/a.md"));
        assert!(glob_matches("file{1..3}.txt", "file2.txt"));
        assert!(!glob_matches("file{1..3}.txt", "file4.txt"));
        assert!(glob_matches("file{3..1}.txt", "file1.txt"));
        assert!(glob_matches("f{-2..2}.txt", "f-1.txt") && !glob_matches("f{-2..2}.txt", "f-3.txt"));
        // A huge range is checked, not spelled out.
        assert!(glob_matches("*.{1..100000000000}", "a.99999999999"));
        assert!(!glob_matches("*.{1..100000000000}", "a.0"));
        assert!(glob_matches("[!a]*.md", "b.md"));
        assert!(!glob_matches("[!a]*.md", "a.md"));
        assert!(glob_matches("*.py", "a.py") && !glob_matches("*.py", "a.pyc"));
    }

    #[test]
    fn reads_nested_files() {
        let dir = std::env::temp_dir().join(format!("kuana-editorconfig-{}", std::process::id()));
        let sub = dir.join("web");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            dir.join(".editorconfig"),
            "root = true\n\n[*]\nindent_style = space\nindent_size = 4\ninsert_final_newline = true\n\n[Makefile]\nindent_style = tab\n",
        )
        .unwrap();
        std::fs::write(sub.join(".editorconfig"), "[*.{js,css}]\nindent_size = 2\ntrim_trailing_whitespace = true\n").unwrap();

        let js = EditorConfig::for_file(&sub.join("app.js"));
        assert_eq!(js.hard_tabs, Some(false));
        assert_eq!(js.indent_size, Some(2));
        assert_eq!(js.insert_final_newline, Some(true));
        assert_eq!(js.trim_trailing_whitespace, Some(true));
        let rs = EditorConfig::for_file(&dir.join("main.rs"));
        assert_eq!(rs.indent_size, Some(4));
        assert_eq!(rs.trim_trailing_whitespace, None);
        let make = EditorConfig::for_file(&dir.join("Makefile"));
        assert_eq!(make.hard_tabs, Some(true));
        std::fs::remove_dir_all(&dir).ok();
    }
}
