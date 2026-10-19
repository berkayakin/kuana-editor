//! Panics written to `<config>/logs/panic-<time>.log`: a panic in a GPUI
//! task aborts the app on macOS (its dispatcher doesn't unwind), and Kuana
//! started from the Dock has no terminal for the message to go to. The next
//! start offers to report the newest one (`offer_report`) as a GitHub issue,
//! filled in from the log, which the user reviews and submits themselves.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::{App, PromptLevel, Window};

/// Writes every panic to a log in `dir` (its message, where it happened and
/// a backtrace), then reports it as before: the hook it replaces runs too.
pub fn install(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let thread = std::thread::current();
        match write(&dir, SystemTime::now(), thread.name().unwrap_or("<unnamed>"), &info.to_string(), &backtrace) {
            Ok(path) => eprintln!("kuana: the panic is written to {}", path.display()),
            Err(err) => eprintln!("kuana: couldn't write the panic to {}: {err}", dir.display()),
        }
        previous(info);
    }));
}

/// Writes one panic (`panic`: "panicked at <place>:\n<message>") to its log
/// in `dir`, after any other in the same second; returns the log's path.
fn write(dir: &Path, at: SystemTime, thread: &str, panic: &str, backtrace: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("panic-{}.log", stamp(at)));
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    writeln!(log, "Kuana {} on {os} {arch}", crate::about::version())?;
    writeln!(log, "thread '{thread}' {panic}\n\nBacktrace:\n{backtrace}")?;
    Ok(path)
}

/// `at` in UTC as `YYYY-MM-DD-HHMMSS`, so logs sort by time.
fn stamp(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, time) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970 to a date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}-{:02}{:02}{:02}", time / 3600, time / 60 % 60, time % 60)
}

/// Names the newest log already offered for reporting.
const SEEN: &str = "seen";
/// Older logs aren't brought up: they're likely from a version since fixed.
const RECENT: Duration = Duration::from_secs(14 * 86_400);
/// GitHub refuses longer issue links; the backtrace is cut to fit.
const MAX_URL: usize = 7_000;

/// The newest panic log in `dir` from the last `RECENT` that no start has
/// offered yet.
fn unseen(dir: &Path, now: SystemTime) -> Option<PathBuf> {
    let seen = std::fs::read_to_string(dir.join(SEEN)).unwrap_or_default();
    let oldest = format!("panic-{}.log", stamp(now.checked_sub(RECENT).unwrap_or(UNIX_EPOCH)));
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("panic-") && name.ends_with(".log"))
        .filter(|name| *name >= oldest && name.as_str() > seen.trim())
        .max()
        .map(|name| dir.join(name))
}

/// After a panic in an earlier run, asks whether to report it: Report opens
/// a new GitHub issue filled in from the log, Show Log reveals the file.
/// Each log is offered once.
pub fn offer_report(dir: &Path, window: &mut Window, cx: &mut App) {
    let Some(path) = unseen(dir, SystemTime::now()) else { return };
    let Ok(log) = std::fs::read_to_string(&path) else { return };
    if let Some(name) = path.file_name() {
        let _ = std::fs::write(dir.join(SEEN), name.as_encoded_bytes());
    }
    let panic = Report::parse(&log);
    let detail = format!(
        "{}\n\nReporting it helps get it fixed. The report opens in your browser, filled in from the log; \
         nothing is sent until you submit it there.",
        panic.summary(),
    );
    let answer = window.prompt(PromptLevel::Warning, "Kuana ran into a bug last time", Some(&detail), &["Report on GitHub…", "Show Log", "Not Now"], cx);
    cx.spawn(async move |cx| match answer.await {
        Ok(0) => cx.update(|cx| cx.open_url(&panic.issue_url(&home()))).ok(),
        Ok(1) => cx.update(|cx| cx.reveal_path(&path)).ok(),
        _ => None,
    })
    .detach();
}

fn home() -> String {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default()
}

/// What a bug report needs from a panic log (its first panic).
#[derive(Debug, PartialEq)]
struct Report {
    /// "Kuana 0.1.10 (7bde937)".
    version: String,
    /// "panicked at src/a.rs:1:2:\nboom".
    message: String,
    /// Kuana's and GPUI's frames: the rest is the standard library and the
    /// panic machinery.
    frames: Vec<String>,
}

impl Report {
    fn parse(log: &str) -> Report {
        let mut lines = log.lines();
        let header = lines.next().unwrap_or_default();
        let version = header.split(" on ").next().unwrap_or(header).to_string();
        let rest: Vec<&str> = lines.collect();
        let start = rest.iter().position(|l| l.starts_with("thread '")).unwrap_or(0);
        let end = rest[start..].iter().position(|l| *l == "Backtrace:").map_or(rest.len(), |i| start + i);
        let message = rest[start..end].join("\n").trim().to_string();
        let message = message.split_once("' ").map_or(message.as_str(), |(_, m)| m).to_string();
        // The first panic's backtrace: up to the next panic's "Kuana …" header.
        let mut frames = Vec::new();
        let mut keep = false;
        for line in rest.iter().skip(end + 1).take_while(|l| !l.starts_with("Kuana ")) {
            let trimmed = line.trim_start();
            if trimmed.starts_with("at ") {
                if keep {
                    frames.push(line.to_string());
                }
            } else {
                keep = ["kuana::", "gpui::"].iter().any(|k| trimmed.contains(k)) && !trimmed.contains("panic_log::");
                if keep {
                    frames.push(line.to_string());
                }
            }
        }
        Report { version, message, frames }
    }

    /// The panic's message without its place ("boom"), else the place.
    fn summary(&self) -> &str {
        self.message.lines().nth(1).or(self.message.lines().next()).unwrap_or_default()
    }

    /// A link to a new issue from the bug report form, filled in. `home` is
    /// shown as `~`, so paths don't carry the user's name.
    fn issue_url(&self, home: &str) -> String {
        let private = |text: &str| if home.len() > 1 { text.replace(home, "~") } else { text.to_string() };
        let title = format!("Crash: {}", self.summary().chars().take(80).collect::<String>());
        let os = match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            _ => "Linux",
        };
        let what = "Kuana hit a bug and quit (from its panic log).\n\nWhat I was doing just before: ";
        let fields = |frames: &[String]| {
            let extra = format!("```\n{}\n\nBacktrace:\n{}\n```", private(&self.message), private(&frames.join("\n")));
            let pairs = [("template", "bug_report.yml"), ("title", &title), ("what", what), ("version", &self.version), ("os", os), ("extra", &extra)];
            let query: Vec<String> =
                pairs.iter().map(|(k, v)| format!("{k}={}", url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>())).collect();
            format!("https://github.com/{}/issues/new?{}", crate::updater::REPO, query.join("&"))
        };
        let mut frames = self.frames.len();
        loop {
            let url = fields(&self.frames[..frames]);
            if url.len() <= MAX_URL || frames == 0 {
                return url;
            }
            frames -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn logs_are_named_by_their_time() {
        let at = |secs: u64| stamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01-000000");
        assert_eq!(at(951_868_800), "2000-03-01-000000");
        assert_eq!(at(1_709_251_199), "2024-02-29-235959");
        assert_eq!(at(1_792_255_025), "2026-10-17-163705");
    }

    #[test]
    fn a_panic_goes_to_its_log() {
        // A folder of the test's own, never the user's config.
        let dir = std::env::temp_dir().join(format!("kuana-panic-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = UNIX_EPOCH + Duration::from_secs(1_792_255_025);
        let path = write(&dir, at, "main", "panicked at src/a.rs:1:2:\nboom", "0: kuana::a").unwrap();
        assert_eq!(path, dir.join("panic-2026-10-17-163705.log"));
        // Another in the same second goes after it.
        write(&dir, at, "worker", "panicked at src/b.rs:3:4:\nagain", "0: kuana::b").unwrap();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.starts_with(&format!("Kuana {}", crate::about::version())), "{log}");
        assert!(log.contains("thread 'main' panicked at src/a.rs:1:2:\nboom\n\nBacktrace:\n0: kuana::a\n"), "{log}");
        assert!(log.contains("thread 'worker' panicked at src/b.rs:3:4:\nagain"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook is the whole process's, so it's tried in a test process of
    /// its own: a panic leaves its log, and the hook before still reports it.
    #[test]
    fn the_hook_logs_a_panic_and_still_reports_it() {
        let dir = std::env::temp_dir().join(format!("kuana-panic-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "panic_log::tests::panics_with_the_hook", "--ignored", "--nocapture", "--test-threads=1"])
            .env("KUANA_PANIC_LOG_TEST", &dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(stderr.contains("on purpose") && stderr.contains("the panic is written to"), "{stderr}");
        let logs: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        let [log] = logs.as_slice() else { panic!("{logs:?}") };
        let log = std::fs::read_to_string(log).unwrap();
        // The location as the platform writes it (`src\panic_log.rs` on Windows).
        let location = std::path::Path::new("src").join("panic_log.rs");
        assert!(log.contains(&format!("thread 'panic_log::tests::panics_with_the_hook' panicked at {}:", location.display())), "{log}");
        assert!(log.contains("on purpose\n\nBacktrace:\n"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    const LOG: &str = "Kuana 0.1.10 (7bde937) on macos aarch64\n\
        thread 'main' panicked at src/git_log.rs:509:45:\n\
        index out of bounds: the len is 0 but the index is 0\n\
        \n\
        Backtrace:\n   \
        0: std::backtrace::Backtrace::create\n   \
        1: kuana::panic_log::install::{{closure}}\n   \
        2: kuana::git_log::GitLog::render\n             \
        at /Users/someone/dev/kuana/src/git_log.rs:509:45\n   \
        3: gpui::window::Window::draw\n   \
        4: core::ops::function::FnOnce::call_once\n\
        Kuana 0.1.10 (7bde937) on macos aarch64\n\
        thread 'worker' panicked at src/b.rs:1:1:\nlater\n\nBacktrace:\n   0: kuana::b\n";

    #[test]
    fn a_report_takes_the_first_panic_and_kuanas_frames() {
        let report = Report::parse(LOG);
        assert_eq!(report.version, "Kuana 0.1.10 (7bde937)");
        assert_eq!(report.message, "panicked at src/git_log.rs:509:45:\nindex out of bounds: the len is 0 but the index is 0");
        assert_eq!(report.summary(), "index out of bounds: the len is 0 but the index is 0");
        let frames: Vec<&str> = report.frames.iter().map(|f| f.trim()).collect();
        assert_eq!(
            frames,
            ["2: kuana::git_log::GitLog::render", "at /Users/someone/dev/kuana/src/git_log.rs:509:45", "3: gpui::window::Window::draw"]
        );
    }

    #[test]
    fn the_issue_link_fills_the_form_without_the_home_folder() {
        let url = Report::parse(LOG).issue_url("/Users/someone");
        assert!(url.starts_with("https://github.com/berkayakin/kuana-editor/issues/new?template=bug_report.yml&title=Crash%3A+index+out+of+bounds"), "{url}");
        let decoded: String = url::form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        assert!(decoded.contains("version=Kuana 0.1.10 (7bde937)\n"), "{decoded}");
        assert!(decoded.contains("at ~/dev/kuana/src/git_log.rs:509:45"), "{decoded}");
        assert!(!decoded.contains("someone"), "{decoded}");
        // A huge backtrace is cut to fit GitHub's limit.
        let mut long = Report::parse(LOG);
        long.frames = (0..2000).map(|i| format!("{i}: kuana::frame_{i}")).collect();
        assert!(long.issue_url("").len() <= MAX_URL);
    }

    #[test]
    fn each_recent_log_is_offered_once() {
        let dir = std::env::temp_dir().join(format!("kuana-panic-unseen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now = UNIX_EPOCH + Duration::from_secs(1_792_255_025);
        assert_eq!(unseen(&dir, now), None);
        // One from a month ago is too old to bring up.
        write(&dir, now - Duration::from_secs(30 * 86_400), "main", "panicked at a", "").unwrap();
        assert_eq!(unseen(&dir, now), None);
        let recent = write(&dir, now - Duration::from_secs(60), "main", "panicked at b", "").unwrap();
        assert_eq!(unseen(&dir, now), Some(recent.clone()));
        std::fs::write(dir.join(SEEN), recent.file_name().unwrap().as_encoded_bytes()).unwrap();
        assert_eq!(unseen(&dir, now), None);
        let newer = write(&dir, now, "main", "panicked at c", "").unwrap();
        assert_eq!(unseen(&dir, now), Some(newer));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Run by the test above, in its own process.
    #[test]
    #[ignore = "run by the_hook_logs_a_panic_and_still_reports_it"]
    fn panics_with_the_hook() {
        let Some(dir) = std::env::var_os("KUANA_PANIC_LOG_TEST") else { return };
        install(PathBuf::from(dir));
        panic!("on purpose");
    }
}
