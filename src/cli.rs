//! Opening Kuana from outside: Finder's Open With and double-clicks (macOS
//! open-file events, as `file://` URLs) and the `kuana` shell command, a small
//! script that hands its paths to the app with `open`, so they reach the
//! running Kuana the same way.

use std::path::{Path, PathBuf};

/// Where Install Shell Command puts the script.
pub const COMMAND_PATH: &str = "/usr/local/bin/kuana";

/// The local paths among URLs macOS asked Kuana to open.
pub fn paths_from_urls(urls: &[String]) -> Vec<PathBuf> {
    urls.iter()
        .filter_map(|u| url::Url::parse(u).ok())
        .filter(|u| u.scheme() == "file")
        .filter_map(|u| u.to_file_path().ok())
        .collect()
}

/// Lists the recent projects where the system lists an app's recent items,
/// to open Kuana in one from there: the Dock icon's menu on macOS (also while
/// Kuana isn't running; a click comes back as an open-file event) and the
/// taskbar's jump list on Windows (a click starts `kuana <folder>`). Linux
/// desktops have no such list. Returns the folders the user removed from the
/// jump list, for Kuana's list to drop too.
pub fn show_recent_projects(projects: &[PathBuf], cx: &mut gpui::App) -> Vec<PathBuf> {
    let projects: Vec<PathBuf> = projects.iter().filter(|p| p.is_dir()).cloned().collect();
    // Not from tests: the Dock's list is the user's.
    #[cfg(all(target_os = "macos", not(test)))]
    show_in_dock(projects.clone(), cx);
    let entries = projects.into_iter().map(|p| std::iter::once(p).collect()).collect();
    cx.update_jump_list(Vec::new(), entries).into_iter().flatten().collect()
}

/// Replaces the system's recent items for Kuana, which the Dock lists.
#[cfg(all(target_os = "macos", not(test)))]
fn show_in_dock(projects: Vec<PathBuf>, cx: &mut gpui::App) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Which call last changed the list.
    static SHOWN: AtomicUsize = AtomicUsize::new(0);
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    // Cleared first, so folders Kuana dropped or that were deleted go too.
    // Safety: the sender can be nil; it's for menu commands.
    unsafe { objc2_app_kit::NSDocumentController::sharedDocumentController(mtm).clearRecentDocuments(None) };
    add_to_dock(&projects, cx);
    // Cleared at launch, the list loses the first folder added after it;
    // adding them again a moment later (unless the list changed meanwhile)
    // puts it back in its place.
    let shown = SHOWN.fetch_add(1, Ordering::Relaxed) + 1;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
        if SHOWN.load(Ordering::Relaxed) == shown {
            cx.update(|cx| add_to_dock(&projects, cx)).ok();
        }
    })
    .detach();
}

/// Oldest first: each one goes to the end, and the Dock shows the newest at
/// the bottom, nearest the icon (like VS Code), keeping the newest when the
/// system's limit on recent items is lower.
#[cfg(all(target_os = "macos", not(test)))]
fn add_to_dock(projects: &[PathBuf], cx: &mut gpui::App) {
    for path in projects.iter().rev() {
        cx.add_recent_document(path);
    }
}

/// The `kuana` script for this copy of Kuana: through `open` when it runs from
/// an app bundle (paths reach the running app), else the binary itself.
pub fn script(exe: &Path) -> String {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
    let app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"));
    let run = match app {
        Some(app) => format!("exec open -a {} \"$@\"", quote(app)),
        None => format!("{} \"$@\" >/dev/null 2>&1 &", quote(exe)),
    };
    // `kuana extension …` runs here, in the terminal.
    let command = format!("[ \"$1\" = extension ] && exec {} \"$@\"", quote(exe));
    format!("#!/bin/sh\n# Opens files and folders in Kuana: kuana [path…]. Installed by Kuana.\n{command}\n{run}\n")
}

/// `kuana extension check [folder]`, before any window opens. Returns the
/// exit code.
pub fn extension_command(args: &[String]) -> i32 {
    match args {
        [check] if check == "check" => crate::extensions::check::run(Path::new(".")),
        [check, dir] if check == "check" => crate::extensions::check::run(Path::new(dir)),
        _ => {
            eprintln!("usage: kuana extension check [folder]\n  checks an extension's manifest, grammars and queries");
            2
        }
    }
}

/// Writes the script to [`COMMAND_PATH`], asking for an administrator
/// password when that folder isn't writable (as VS Code does).
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let text = script(&exe);
    let target = Path::new(COMMAND_PATH);
    let direct = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(target, &text));
    if direct.is_ok() {
        return set_executable(target);
    }
    if !cfg!(target_os = "macos") {
        return Err(format!("couldn't write {COMMAND_PATH}: {}", direct.unwrap_err()));
    }
    let staged = std::env::temp_dir().join(format!("kuana-command-{}", std::process::id()));
    std::fs::write(&staged, &text).map_err(|e| e.to_string())?;
    let shell = format!(
        "mkdir -p /usr/local/bin && cp '{}' {COMMAND_PATH} && chmod 755 {COMMAND_PATH}",
        staged.display()
    );
    let apple_script = format!("do shell script \"{shell}\" with administrator privileges");
    let output = crate::process::command("osascript").arg("-e").arg(apple_script).output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&staged);
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.contains("-128") { "Cancelled".into() } else { message })
    }
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_and_urls() {
        let bundled = script(Path::new("/Applications/Kuana's.app/Contents/MacOS/kuana"));
        assert!(bundled.contains(r#"exec open -a '/Applications/Kuana'\''s.app' "$@""#), "{bundled}");
        let dev = script(Path::new("/src/kuana/target/release/kuana"));
        assert!(dev.contains(r#"'/src/kuana/target/release/kuana' "$@""#), "{dev}");
        assert!(bundled.contains("[ \"$1\" = extension ] && exec"), "{bundled}");
        let drive = if cfg!(windows) { "C:" } else { "" };
        let folder = format!("{}{drive}/Users/a/My%20Project/", if cfg!(windows) { "file:///" } else { "file://" });
        let urls = vec![folder, "https://example.com".to_string()];
        assert_eq!(paths_from_urls(&urls), vec![PathBuf::from(format!("{drive}/Users/a/My Project/"))]);
    }
}
