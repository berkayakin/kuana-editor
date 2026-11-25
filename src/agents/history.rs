//! Saved sessions: each one's transcript in `<config>/agents/sessions/<id>.json`
//! and a summary of all of them in `index.json` (agent, folder, title,
//! times), read once and rewritten whole on each change, so listing doesn't
//! read every transcript. Written with `state::write_atomic`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::transcript::Transcript;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub agent: String,
    pub agent_name: String,
    pub cwd: PathBuf,
    pub title: String,
    /// Seconds since the epoch.
    pub created: u64,
    pub updated: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(flatten)]
    pub summary: Summary,
    /// The agent's own id for it, to load it again where the agent can.
    #[serde(default)]
    pub acp_session: Option<String>,
    pub transcript: Transcript,
}

/// Held while the index is read and rewritten; the sessions deleted in this
/// run, which a save that was already on its way mustn't bring back.
static WRITING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn dir() -> Option<PathBuf> {
    crate::acp::install::agents_dir().map(|d| d.join("sessions"))
}

fn file(id: &str) -> Option<PathBuf> {
    let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    Some(dir()?.join(format!("{safe}.json")))
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Every saved session's summary, newest first.
pub fn index() -> Vec<Summary> {
    let Some(path) = dir().map(|d| d.join("index.json")) else { return Vec::new() };
    let mut list: Vec<Summary> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.sort_by_key(|s| std::cmp::Reverse(s.updated));
    list
}

fn write_index(list: &[Summary]) -> std::io::Result<()> {
    let Some(dir) = dir() else { return Ok(()) };
    std::fs::create_dir_all(&dir)?;
    crate::state::write_atomic(&dir.join("index.json"), &serde_json::to_vec(list).unwrap_or_default())
}

/// Saves a session and its summary. One at a time: each rewrites the index.
pub fn save(saved: &Saved) -> std::io::Result<()> {
    let deleted = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if deleted.contains(&saved.summary.id) {
        return Ok(());
    }
    let Some(path) = file(&saved.summary.id) else { return Ok(()) };
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    crate::state::write_atomic(&path, &serde_json::to_vec(saved).unwrap_or_default())?;
    let mut list = index();
    list.retain(|s| s.id != saved.summary.id);
    list.insert(0, saved.summary.clone());
    write_index(&list)
}

pub fn load(id: &str) -> Option<Saved> {
    serde_json::from_slice(&std::fs::read(file(id)?).ok()?).ok()
}

pub fn delete(id: &str) -> std::io::Result<()> {
    // What Kuana's own agent kept of it goes too.
    if let Some(acp) = load(id).and_then(|s| s.acp_session) {
        super::kuana_agent::forget(&acp);
    }
    let mut deleted = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    deleted.push(id.to_string());
    if let Some(path) = file(id) {
        let _ = std::fs::remove_file(path);
    }
    let mut list = index();
    list.retain(|s| s.id != id);
    write_index(&list)
}

/// A title from a first prompt: its first line, cut at a word near 60 characters.
pub fn title_from(prompt: &str) -> String {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New session");
    if line.chars().count() <= 60 {
        return line.to_string();
    }
    let cut: String = line.chars().take(60).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::transcript::Entry;

    #[test]
    fn sessions_save_list_load_and_delete() {
        let summary = |id: &str, updated| Summary {
            id: id.into(),
            agent: "kuana".into(),
            agent_name: "Kuana Agent".into(),
            cwd: "/p".into(),
            title: "Fix the build".into(),
            created: 1,
            updated,
        };
        let mut transcript = Transcript::default();
        transcript.push(Entry::User { text: "Fix the build".into(), context: vec![] });
        let saved = Saved { summary: summary("history-test-a", 10), acp_session: Some("s1".into()), transcript };
        save(&saved).unwrap();
        save(&Saved { summary: summary("history-test-b", 20), acp_session: None, transcript: Transcript::default() }).unwrap();
        let ids: Vec<String> = index().into_iter().map(|s| s.id).filter(|id| id.starts_with("history-test")).collect();
        assert_eq!(ids, vec!["history-test-b".to_string(), "history-test-a".to_string()]);
        // The generation counter isn't saved (it counts changes in this run).
        let loaded = load("history-test-a").unwrap();
        assert_eq!((loaded.summary, loaded.acp_session, loaded.transcript.entries), (saved.summary, saved.acp_session, saved.transcript.entries));
        delete("history-test-a").unwrap();
        delete("history-test-b").unwrap();
        assert!(load("history-test-a").is_none());
        assert!(index().iter().all(|s| !s.id.starts_with("history-test")));
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(title_from("\n  Fix the build\nand more"), "Fix the build");
        let long = "Refactor the parser so that it handles nested generics without recursion blowing up";
        let title = title_from(long);
        assert!(title.ends_with('…') && title.chars().count() <= 61, "{title}");
        assert_eq!(title_from(""), "New session");
    }
}
