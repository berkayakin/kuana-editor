//! WSL (`connection::Transport::Wsl`): Linux distributions on Windows,
//! reached with `wsl.exe -d <distro> --exec sh -c …`, so the server runs in
//! the distribution as its default user, like VS Code's WSL extension.

use std::process::Stdio;

use super::connection::Transport;

/// The installed distributions, the default one first (`wsl.exe -l -q`).
pub fn distros() -> Result<Vec<String>, String> {
    let program = Transport::Wsl(String::new()).default_program();
    // UTF-8 when WSL takes `WSL_UTF8`, UTF-16 otherwise (`decode`).
    let out = crate::process::command(&program).args(["--list", "--quiet"]).env("WSL_UTF8", "1").stdin(Stdio::null()).output().map_err(|e| format!("Couldn't run {program}: {e}"))?;
    if !out.status.success() {
        return Err(super::connection::tool_error(&decode(&out.stderr).into_bytes(), "WSL didn't list its distributions"));
    }
    Ok(names(&decode(&out.stdout)))
}

/// `wsl.exe`'s output: UTF-16LE (with or without a byte order mark) from
/// older WSLs, else UTF-8.
fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
    // ASCII as UTF-16LE: every other byte is zero.
    let utf16 = bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count() * 2 >= bytes.len() / 2;
    if utf16 {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn names(text: &str) -> Vec<String> {
    // Docker Desktop's own distributions aren't for working in.
    text.lines().map(|l| l.trim().trim_matches('\0')).filter(|l| !l.is_empty() && !l.starts_with("docker-desktop")).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_are_read_in_either_encoding() {
        let listed = "Ubuntu-24.04\r\ndocker-desktop\r\nDebian\r\n";
        let utf16: Vec<u8> = listed.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let with_bom: Vec<u8> = [0xff, 0xfe].into_iter().chain(utf16.iter().copied()).collect();
        for bytes in [listed.as_bytes().to_vec(), utf16, with_bom] {
            assert_eq!(names(&decode(&bytes)), ["Ubuntu-24.04", "Debian"]);
        }
    }
}
