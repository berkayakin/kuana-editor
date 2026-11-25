//! Prompts sent to agents, with the project folder each was sent in: ↑ in a
//! session's empty composer brings them back, newest first (as Claude Code's
//! prompt does). Kept in `<config>/agents/prompts.json` (a temp folder under
//! test), at most `MAX` of them, rewritten whole on each one sent: read
//! once, small.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The most prompts kept, all projects together.
const MAX: usize = 500;
/// Longer prompts (a pasted log) aren't kept.
const MAX_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Sent {
    folder: PathBuf,
    text: String,
}

/// Read on first use; oldest first.
static SENT: Mutex<Option<Vec<Sent>>> = Mutex::new(None);

fn file() -> Option<PathBuf> {
    crate::acp::install::agents_dir().map(|d| d.join("prompts.json"))
}

fn with<T>(f: impl FnOnce(&mut Vec<Sent>) -> T) -> T {
    let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
    let list = sent.get_or_insert_with(|| file().and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default());
    f(list)
}

/// Reads the list now (off the UI thread), so the first ↑ needn't.
pub fn load() {
    with(|_| ());
}

/// Notes a prompt sent in `folder` (a repeat moves to the newest). Writes the
/// file: call it off the UI thread.
pub fn record(folder: &Path, text: &str) {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_BYTES {
        return;
    }
    with(|list| {
        list.retain(|s| !(s.folder == folder && s.text == text));
        list.push(Sent { folder: folder.to_path_buf(), text: text.to_string() });
        if list.len() > MAX {
            list.drain(..list.len() - MAX);
        }
        // Written while held, so two prompts sent at once land in order.
        let Some(path) = file() else { return };
        let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::state::write_atomic(&path, &serde_json::to_vec(list).unwrap_or_default()));
        if let Err(err) = written {
            eprintln!("couldn't save the prompts sent: {err}");
        }
    });
}

/// The prompts sent in `folder`, newest first.
pub fn sent_in(folder: &Path) -> Vec<String> {
    with(|list| list.iter().rev().filter(|s| s.folder == folder).map(|s| s.text.clone()).collect())
}

/// What ↑ or ↓ does in the composer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Recall {
    /// Show the prompt at this index of the list (newest first).
    Show(usize),
    /// Back to an empty composer (↓ past the newest).
    Clear,
    /// The key is taken and nothing changes (↑ at the oldest).
    Stay,
    /// Not for recalling: the key moves the caret as usual.
    Pass,
}

/// ↑ (`older`) or ↓ in the composer, which shows the prompt at `showing`
/// (unedited) or something else, of `len` earlier prompts. Prompts come back
/// only into an empty composer, so nothing typed is lost; within a recalled
/// prompt of several lines the keys move the caret until it's on the first
/// line (↑) or the last (↓).
pub fn step(showing: Option<usize>, len: usize, empty: bool, on_edge_line: bool, older: bool) -> Recall {
    match (showing, older) {
        (None, true) if empty && len > 0 => Recall::Show(0),
        (None, _) => Recall::Pass,
        (Some(_), _) if !on_edge_line => Recall::Pass,
        (Some(at), true) if at + 1 < len => Recall::Show(at + 1),
        (Some(_), true) => Recall::Stay,
        (Some(0), false) => Recall::Clear,
        (Some(at), false) => Recall::Show(at - 1),
    }
}

/// The prompts ↑ goes through: the session's own (newest first), then the
/// project's others, each once.
pub fn recallable(own: impl DoubleEndedIterator<Item = String>, folder: &Path) -> Vec<String> {
    let mut list: Vec<String> = Vec::new();
    for text in own.rev().chain(sent_in(folder)) {
        let text = text.trim().to_string();
        if !text.is_empty() && !list.contains(&text) {
            list.push(text);
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_come_back_by_folder_newest_first_once() {
        let (a, b) = (Path::new("/kuana-test/prompts/a"), Path::new("/kuana-test/prompts/b"));
        record(a, "first");
        record(b, "elsewhere");
        record(a, "  second  ");
        record(a, "first");
        record(a, "");
        assert_eq!(sent_in(a), ["first", "second"]);
        assert_eq!(sent_in(b), ["elsewhere"]);
        // The session's own come first, without repeats.
        let own = vec!["mine".to_string(), "second".to_string()];
        assert_eq!(recallable(own.into_iter(), a), ["second", "mine", "first"]);
        // Too long to keep.
        record(a, &"x".repeat(MAX_BYTES + 1));
        assert_eq!(sent_in(a).len(), 2);
    }

    #[test]
    fn up_and_down_walk_the_prompts_without_losing_text() {
        // ↑ in an empty composer: the newest; with text typed: the caret moves.
        assert_eq!(step(None, 3, true, true, true), Recall::Show(0));
        assert_eq!(step(None, 3, false, true, true), Recall::Pass);
        assert_eq!(step(None, 0, true, true, true), Recall::Pass);
        assert_eq!(step(None, 3, true, true, false), Recall::Pass);
        // Through them and back.
        assert_eq!(step(Some(0), 3, false, true, true), Recall::Show(1));
        assert_eq!(step(Some(2), 3, false, true, true), Recall::Stay);
        assert_eq!(step(Some(2), 3, false, true, false), Recall::Show(1));
        assert_eq!(step(Some(0), 3, false, true, false), Recall::Clear);
        // A recalled prompt of several lines: the caret moves inside it first.
        assert_eq!(step(Some(1), 3, false, false, true), Recall::Pass);
        assert_eq!(step(Some(1), 3, false, false, false), Recall::Pass);
    }
}
