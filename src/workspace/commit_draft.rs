//! The Commit view's AI-written message: what the commit would take (all
//! tracked changes, or the staged ones and the unversioned files checked)
//! goes to the model chosen in Settings → AI with the repository's recent
//! subjects, so the style matches, and what was typed in the box as notes.
//! The message streams into the box; Stop (the same button) ends it. The
//! prompt is `ai::tasks::commit_message`.

use super::*;
use crate::ai::{StopReason, StreamEvent};
use futures::StreamExt as _;

/// How often the message box follows the reply.
const FOLLOW_EVERY: Duration = Duration::from_millis(100);
/// The most of an unversioned file sent.
const NEW_FILE_BYTES: usize = 4_000;

impl Workspace {
    /// Write Commit Message with AI: the Commit view shows, its button pressed.
    pub(super) fn write_commit_message(&mut self, _: &WriteCommitMessage, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_repo(cx) {
            self.toast(ToastKind::Info, "The project isn't in a Git repository", None, cx);
            return;
        }
        self.show_commit(&ShowCommit, window, cx);
        self.git_panel.update(cx, |p, cx| p.draft_message(cx));
    }

    pub(super) fn draft_commit_message(&mut self, all: bool, untracked: Vec<String>, notes: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo(cx) else { return };
        let choice = match Settings::get(cx).ai.clone().unwrap_or_default().choice() {
            Ok(choice) => choice,
            Err(why) => {
                self.toast(ToastKind::Info, "Choose an AI model to write commit messages", Some(why), cx);
                cx.defer(|cx| crate::preferences::open(Some(crate::preferences::Page::Ai), cx));
                return;
            }
        };
        let panel = self.git_panel.clone();
        panel.update(cx, |p, cx| p.set_drafting(true, cx));
        let read = cx.background_executor().spawn(async move { what_is_committed(&repo, all, &untracked) });
        self.commit_draft = Some(cx.spawn(async move |ws, cx| {
            let (diff, recent, branch) = read.await;
            if diff.trim().is_empty() {
                ws.update(cx, |ws, cx| {
                    ws.toast(ToastKind::Info, "Nothing to describe yet", Some("Stage changes, or change tracked files, first.".into()), cx);
                    ws.commit_draft_ended(cx);
                })
                .ok();
                return;
            }
            let mut stream = choice.stream(crate::ai::tasks::commit_message(&diff, &recent, branch.as_deref(), &notes));
            let (mut reply, mut error, mut stop) = (String::new(), None, StopReason::EndTurn);
            let mut shown = std::time::Instant::now();
            while let Some(event) = stream.events.next().await {
                match event {
                    StreamEvent::Text { text, .. } => reply.push_str(&text),
                    StreamEvent::Stop(reason) => stop = reason,
                    StreamEvent::Error(message) => error = Some(message),
                    _ => {}
                }
                if shown.elapsed() >= FOLLOW_EVERY && !reply.trim().is_empty() {
                    shown = std::time::Instant::now();
                    let text = crate::ai::tasks::commit_text(&reply);
                    if panel.update(cx, |p, cx| p.set_message(&text, cx)).is_err() {
                        return;
                    }
                }
            }
            let message = crate::ai::tasks::commit_text(&reply);
            ws.update(cx, |ws, cx| {
                let failure = match (error, &stop) {
                    (Some(message), _) => Some(message),
                    (None, StopReason::Refusal) => Some("The model declined this request.".to_string()),
                    (None, _) if message.is_empty() => Some("The model sent nothing back.".to_string()),
                    _ => None,
                };
                match failure {
                    Some(why) => {
                        // What was typed comes back.
                        ws.git_panel.update(cx, |p, cx| p.set_message(&notes, cx));
                        ws.toast(ToastKind::Error, "Couldn't write the commit message", Some(why), cx);
                    }
                    None => ws.git_panel.update(cx, |p, cx| p.set_message(&message, cx)),
                }
                ws.commit_draft_ended(cx);
            })
            .ok();
        }));
    }

    /// Stop in the message box, or the end of the reply.
    pub(super) fn commit_draft_ended(&mut self, cx: &mut Context<Self>) {
        self.commit_draft = None;
        self.git_panel.update(cx, |p, cx| p.set_drafting(false, cx));
    }
}

/// The diff a commit would take, the repository's recent subjects and its
/// branch (off the UI thread).
fn what_is_committed(repo: &Path, all: bool, untracked: &[String]) -> (String, Vec<String>, Option<String>) {
    let has_head = git::run(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    let mut diff = if all && has_head {
        git::run(repo, &["diff", "HEAD", "--no-color", "--no-ext-diff"]).unwrap_or_default()
    } else {
        git::run(repo, &["diff", "--cached", "--no-color", "--no-ext-diff"]).unwrap_or_default()
    };
    for rel in untracked {
        let Ok(bytes) = crate::fs::read(repo.join(rel)) else { continue };
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(NEW_FILE_BYTES)]).into_owned();
        let more = if bytes.len() > NEW_FILE_BYTES { "\n[… the rest of the file left out]" } else { "" };
        diff.push_str(&format!("\nnew file {rel}:\n{text}{more}\n"));
    }
    let recent = if has_head { git::run(repo, &["log", "-15", "--format=%s"]).unwrap_or_default().lines().map(str::to_string).collect() } else { Vec::new() };
    let branch = git::run(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().map(|b| b.trim().to_string()).filter(|b| !b.is_empty() && b != "HEAD");
    (diff, recent, branch)
}

#[cfg(test)]
mod tests {
    /// The diff, subjects and branch come from a real repository.
    #[test]
    fn commits_are_described_from_the_repository() {
        if !crate::git::is_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!("kuana-commit-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::git::init(&root).unwrap();
        let root = crate::git::discover(&root).unwrap();
        for args in [["config", "user.name", "Test"], ["config", "user.email", "test@example.com"], ["checkout", "-b", "feature"]] {
            crate::git::run(&root, &args).unwrap();
        }
        // No commits yet: the staged file is the diff.
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        crate::git::run(&root, &["add", "a.txt"]).unwrap();
        let (diff, recent, _) = super::what_is_committed(&root, false, &[]);
        assert!(diff.contains("+one"), "{diff}");
        assert!(recent.is_empty());
        crate::git::run(&root, &["commit", "-m", "Add a"]).unwrap();

        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        let (diff, recent, branch) = super::what_is_committed(&root, true, &["new.txt".into()]);
        assert!(diff.contains("-one\n+two"), "{diff}");
        assert!(diff.contains("new file new.txt:\nfresh"), "{diff}");
        assert_eq!(recent, ["Add a"]);
        assert_eq!(branch.as_deref(), Some("feature"));
        // Only what's staged when that's what the commit takes.
        let (diff, ..) = super::what_is_committed(&root, false, &[]);
        assert!(diff.is_empty(), "{diff}");
        std::fs::remove_dir_all(&root).ok();
    }
}
