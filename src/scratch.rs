//! Scratch files: throwaway files in any language, kept between runs in
//! `<config>/scratches/` (not in any project), named `scratch_<n>.<suffix>`.
//! They open as ordinary file tabs, so highlighting, language servers, Save
//! and auto-save work as for any file; a script language gets a ▶ on the
//! first line that runs the file (`run_target`).

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    run::{DebugSpec, RunTarget, chain, shell_quote},
    syntax::Language,
};

/// Where scratch files live.
pub fn dir() -> Option<PathBuf> {
    let dir = crate::theme::config_dir()?.join("scratches");
    // Opened files' paths are canonical (a linked config folder).
    Some(crate::paths::canonicalize(&dir).unwrap_or(dir))
}

/// Whether `path` is a scratch file.
pub fn is_scratch(path: &Path) -> bool {
    dir().is_some_and(|d| path.parent() == Some(d.as_path()))
}

/// A language a scratch file can be in.
pub struct ScratchLanguage {
    pub name: String,
    pub suffix: String,
}

/// Built-in languages, then extensions', then plain text; each once by name.
pub fn languages() -> Vec<ScratchLanguage> {
    let mut out: Vec<ScratchLanguage> = crate::languages::all()
        .iter()
        .filter_map(|d| Some(ScratchLanguage { name: d.name.to_string(), suffix: d.file_suffix()?.to_string() }))
        .collect();
    for ix in crate::extensions::languages() {
        let Some(def) = crate::extensions::language(ix) else { continue };
        let Some(suffix) = def.file_suffix() else { continue };
        if !out.iter().any(|l| l.name.eq_ignore_ascii_case(def.name)) {
            out.push(ScratchLanguage { name: def.name.to_string(), suffix: suffix.to_string() });
        }
    }
    out.push(ScratchLanguage { name: "Plain Text".into(), suffix: "txt".into() });
    out
}

/// The number in a scratch file's name (`scratch_12.py` → 12).
fn number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("scratch_")?;
    rest.split('.').next()?.parse().ok()
}

/// The next scratch file's name: one past the highest number in use, so a
/// deleted scratch's number isn't given to a new one in the same run.
pub fn next_name<'a>(existing: impl IntoIterator<Item = &'a str>, suffix: &str) -> String {
    let n = existing.into_iter().filter_map(number).max().unwrap_or(0) + 1;
    format!("scratch_{n}.{suffix}")
}

/// Makes an empty scratch file with `suffix`.
pub fn create(suffix: &str) -> std::io::Result<PathBuf> {
    let dir = dir().ok_or_else(|| std::io::Error::other("no config folder"))?;
    std::fs::create_dir_all(&dir)?;
    let names: Vec<String> = std::fs::read_dir(&dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let path = dir.join(next_name(names.iter().map(String::as_str), suffix));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    Ok(path)
}

/// The scratch files, most recently changed first.
pub fn list() -> Vec<(PathBuf, SystemTime)> {
    let Some(dir) = dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            Some((e.path(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Running a scratch file: by its language's interpreter, or compiled
/// first (Rust, into the scratch folder's `.build`), in the scratch folder.
/// None for languages with nothing to run.
pub fn run_target(language: Language, path: &Path, lines: usize) -> Option<RunTarget> {
    let dir = path.parent()?;
    let file = path.to_string_lossy().into_owned();
    let python = if cfg!(windows) { "python" } else { "python3" };
    let argv = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let (command, debug) = match language {
        Language::Python => (
            chain(&[], &argv(&[python, &file])),
            Some(DebugSpec::Python { program: Some(path.to_path_buf()), module: None, args: Vec::new() }),
        ),
        Language::JavaScript => {
            (chain(&[], &argv(&["node", &file])), Some(DebugSpec::Node { runtime: "node".into(), args: vec![file.clone()] }))
        }
        // Node 22.6 and later run TypeScript by stripping its types.
        Language::TypeScript => (chain(&[], &argv(&["node", "--experimental-strip-types", &file])), None),
        Language::Shell => (chain(&[], &argv(&["bash", &file])), None),
        Language::Go => (chain(&[], &argv(&["go", "run", &file])), None),
        Language::Php => (chain(&[], &argv(&["php", &file])), None),
        Language::Rust => {
            let build = dir.join(".build");
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            let program = build.join(if cfg!(windows) { format!("{stem}.exe") } else { stem });
            let program = program.to_string_lossy().into_owned();
            let mkdir = if cfg!(windows) {
                format!("New-Item -ItemType Directory -Force {} | Out-Null; ", shell_quote(&build.to_string_lossy()))
            } else {
                format!("mkdir -p {} && ", shell_quote(&build.to_string_lossy()))
            };
            let compile = argv(&["rustc", "--edition", "2024", "-o", &program, &file]);
            (format!("{mkdir}{}", chain(&[compile], &argv(&[&program]))), None)
        }
        _ => return None,
    };
    let name = path.file_name()?.to_string_lossy();
    Some(RunTarget {
        name: format!("Run '{name}'"),
        command,
        cwd: dir.to_path_buf(),
        row: Some(0),
        rows: 0..lines.max(1),
        debug,
        test: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_the_highest() {
        assert_eq!(next_name([], "py"), "scratch_1.py");
        assert_eq!(next_name(["scratch_1.py", "scratch_7.rs", "notes.md", "scratch_x.txt"], "md"), "scratch_8.md");
        assert_eq!(next_name(["scratch_3"], "txt"), "scratch_4.txt");
    }

    #[test]
    fn every_language_has_a_suffix() {
        let languages = languages();
        let python = languages.iter().find(|l| l.name == "Python").unwrap();
        assert_eq!(python.suffix, "py");
        assert!(languages.iter().all(|l| !l.suffix.is_empty()));
        // Each suffix names its language back.
        for l in languages.iter().filter(|l| l.name != "Plain Text") {
            let path = PathBuf::from(format!("scratch_1.{}", l.suffix));
            assert_eq!(Language::from_path(&path).name(), l.name, "{}", l.suffix);
        }
    }

    #[test]
    fn scripts_run_in_the_scratch_folder() {
        let path = Path::new("/cfg/scratches/scratch_2.py");
        let target = run_target(Language::Python, path, 10).unwrap();
        assert_eq!(target.cwd, Path::new("/cfg/scratches"));
        assert_eq!(target.rows, 0..10);
        assert!(target.command.ends_with("/cfg/scratches/scratch_2.py"), "{}", target.command);
        let rust = run_target(Language::Rust, Path::new("/cfg/scratches/scratch_3.rs"), 3).unwrap();
        assert!(rust.command.contains("rustc"), "{}", rust.command);
        let program = Path::new("/cfg/scratches").join(".build").join("scratch_3");
        assert!(rust.command.contains(&*program.to_string_lossy()), "{}", rust.command);
        assert!(run_target(Language::Markdown, Path::new("/cfg/scratches/scratch_4.md"), 1).is_none());
    }
}
