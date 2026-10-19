//! Markdown linting via rumdl, a Rust implementation of the markdownlint
//! rules (MD001–MD07x). A project's own `.markdownlint.json`/`.yaml`,
//! `.rumdl.toml` or `pyproject.toml` `[tool.rumdl]` is honored.

use std::path::{Path, PathBuf};

use rumdl_lib::config::{Config, SourcedConfig};

use super::{Diagnostic, Severity};

pub fn lint(text: &str, path: Option<&Path>) -> Vec<Diagnostic> {
    let config = load_config(path);
    let rules = rumdl_lib::rules::all_rules(&config);
    let rules = rumdl_lib::rules::filter_rules(&rules, &config.global);
    let flavor = config.markdown_flavor();
    let Ok(warnings) = rumdl_lib::lint(text, &rules, false, flavor, path.map(Path::to_path_buf), Some(&config)) else {
        return Vec::new();
    };

    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    // rumdl positions are 1-based lines and 1-based character columns.
    let offset = |line: usize, column: usize| -> usize {
        let Some(&start) = line_starts.get(line.saturating_sub(1)) else {
            return text.len();
        };
        let end = text[start..].find('\n').map_or(text.len(), |e| start + e);
        text[start..end]
            .char_indices()
            .nth(column.saturating_sub(1))
            .map_or(end, |(i, _)| start + i)
    };

    warnings
        .into_iter()
        .map(|w| {
            let start = offset(w.line, w.column);
            let end = offset(w.end_line.max(w.line), w.end_column).max(start);
            Diagnostic {
                range: start..end,
                severity: match w.severity {
                    rumdl_lib::rule::Severity::Error => Severity::Error,
                    rumdl_lib::rule::Severity::Warning => Severity::Warning,
                    rumdl_lib::rule::Severity::Info => Severity::Info,
                },
                message: w.message.into(),
                rule: w.rule_name.unwrap_or_else(|| "markdown".into()).into(),
                unnecessary: false,
            }
        })
        .collect()
}

/// The project's Markdown lint config, or rumdl's defaults. Without a
/// project config the line-length rule (MD013) is off: hard-wrapping prose
/// at 80 columns is a style choice, and flagging every long line is noise.
fn load_config(path: Option<&Path>) -> Config {
    let Some(dir) = path.and_then(Path::parent) else {
        return default_config();
    };
    let root = project_root(dir);
    SourcedConfig::discover_config_for_dir(dir, &root)
        .and_then(|cfg| SourcedConfig::load_config_for_path(&cfg, &root).ok())
        .unwrap_or_else(default_config)
}

fn default_config() -> Config {
    let mut config = Config::default();
    config.global.disable.push("MD013".into());
    config
}

/// The enclosing repository (a folder with `.git`), or the file's folder.
fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Vec<String> {
        lint(text, None).into_iter().map(|d| d.rule.into_owned()).collect()
    }

    #[test]
    fn reports_common_markdownlint_rules() {
        // Heading levels should only increase by one (MD001).
        assert!(rules("# Title\n\n### Skipped\n").contains(&"MD001".to_string()));
        // Trailing spaces (MD009).
        assert!(rules("# Title\n\nSome text   \n").contains(&"MD009".to_string()));
        // Fenced code blocks need a language (MD040).
        assert!(rules("# Title\n\n```\ncode\n```\n").contains(&"MD040".to_string()));
        // A clean document has no problems; long lines are fine by default.
        let long = "x".repeat(200);
        assert!(rules(&format!("# Title\n\nSome {long} text.\n")).is_empty());
    }

    #[test]
    fn positions_are_byte_ranges() {
        let text = "# Title\n\nÄbc   \n";
        let d = lint(text, None).into_iter().find(|d| d.rule == "MD009").unwrap();
        assert_eq!(&text[d.range.clone()], "   ");
    }

    #[test]
    fn honors_project_config() {
        let dir = std::env::temp_dir().join(format!("kuana-md-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".markdownlint.json"), r#"{ "MD001": false }"#).unwrap();
        let file = dir.join("README.md");
        let found = lint("# Title\n\n### Skipped\n", Some(&file));
        assert!(!found.iter().any(|d| d.rule == "MD001"), "{found:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
