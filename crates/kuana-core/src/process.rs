//! Starting other programs, and ending them.

use std::{
    ffi::OsStr,
    process::{Child, Command, Stdio},
};

/// A command that opens no console window. Kuana is a GUI app on Windows, so
/// each console program it starts (git, shells, servers, compilers) would
/// otherwise flash a window of its own.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Ends `child` and what it started, and waits for it. On Windows a killed
/// process's children keep running, and a program started through a `.cmd`
/// (an npm package's command: language servers, debug adapters) is `cmd.exe`
/// with the program under it, so the whole tree goes (`taskkill /T`).
/// Elsewhere helpers exit once the pipes they share with `child` close.
pub fn kill_tree(child: &mut Child) {
    if cfg!(windows) && child.try_wait().ok().flatten().is_none() {
        let taskkill = std::env::var_os("SystemRoot")
            .map(|root| std::path::Path::new(&root).join("System32").join("taskkill.exe"))
            .filter(|path| path.is_file());
        let ended = command(taskkill.as_deref().unwrap_or("taskkill".as_ref()))
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if ended {
            let _ = child.wait();
            return;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// What runs under a killed program goes with it: here `cmd.exe` with
    /// PowerShell under it, as an npm command's `.cmd` runs Node.
    #[test]
    fn killing_a_program_ends_what_it_started() {
        use base64::Engine;
        use std::time::{Duration, Instant};
        let note = std::env::temp_dir().join(format!("kuana-tree-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&note);
        let script = format!("Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 60", note.display());
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        let mut child = command("cmd").args(["/c", "powershell", "-NoProfile", "-EncodedCommand", &encoded]).spawn().unwrap();
        let started = Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&note).ok().and_then(|t| t.trim().parse::<u32>().ok()) {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "PowerShell didn't start");
            std::thread::sleep(Duration::from_millis(100));
        };
        kill_tree(&mut child);
        let running = |pid: u32| {
            let out = command("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        };
        let started = Instant::now();
        while running(pid) {
            assert!(started.elapsed() < Duration::from_secs(10), "PowerShell outlived the cmd.exe that started it");
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&note);
    }
}
