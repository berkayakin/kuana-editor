//! Helpers the integration tests share.

use std::path::Path;

/// Writes an executable script. Not with `std::fs::write`: on Linux, a
/// program another test thread forks meanwhile inherits the open file until
/// it execs, and running the script then fails with "Text file busy"
/// (ETXTBSY). Written by another process, this one never has it open.
pub fn write_script(path: &Path, text: &str) {
    let source = path.with_extension("source");
    std::fs::write(&source, text).unwrap();
    let status = std::process::Command::new("install").arg("-m").arg("755").arg(&source).arg(path).status().unwrap();
    assert!(status.success(), "install {} failed", path.display());
    std::fs::remove_file(&source).unwrap();
}
