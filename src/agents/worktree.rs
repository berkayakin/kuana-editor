//! Sessions in a Git worktree of their own, as Claude's desktop app can run
//! them: a separate checkout of the project's repository on a new branch,
//! so the agent's edits touch neither the project folder nor other
//! sessions'. Chosen before the first prompt (the agent starts over in it);
//! kept in `<config>/worktrees/<repository>-<hash>/<name>`, outside the
//! project, so its files don't show in the project's tree, searches and
//! file index. The session then commits its changes on the branch, merges it
//! (Kuana's own merge, conflicts included) and removes the worktree.
//! Everything here runs Git: call it off the UI thread.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    /// Where the session works: the project folder's place in the checkout.
    pub path: PathBuf,
    /// The checkout itself (`git worktree add`'s folder).
    pub checkout: PathBuf,
    /// Its branch (`kuana/<name>`).
    pub branch: String,
    /// The commit it started from.
    pub base: String,
    /// The project folder the session belongs to (where it's listed).
    pub project: PathBuf,
    /// The repository's main working tree, where its branch is merged.
    pub repo: PathBuf,
    /// Removed since (Remove Worktree): the session stays, to read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

impl Worktree {
    /// Where a file of the repository is in this checkout (other paths as
    /// they are): what the session is given points into its worktree.
    pub fn map(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.repo).map_or_else(|_| path.to_path_buf(), |rel| self.checkout.join(rel))
    }
}

/// Where worktrees go: Kuana's config folder (a temporary one under test).
fn root() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("kuana-test-worktrees-{}", std::process::id())));
    }
    crate::theme::config_dir().map(|d| d.join("worktrees"))
}

/// A name for a session's worktree and branch: `agent-<the session id's end>`.
pub fn name_for(session_id: &str) -> String {
    let tail: String = session_id.chars().rev().filter(char::is_ascii_alphanumeric).take(6).collect::<Vec<_>>().into_iter().rev().collect();
    format!("agent-{tail}")
}

/// Makes a worktree of the repository holding `project` on a new branch
/// from its HEAD (without the project's uncommitted changes).
pub fn create(project: &Path, name: &str) -> Result<Worktree, String> {
    let repo = crate::git::discover(project).ok_or("The project isn't in a Git repository")?;
    let base = crate::git::run(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|_| "The repository has no commits yet".to_string())?.trim().to_string();
    let repo_name = repo.file_name().map_or_else(|| "repository".to_string(), |n| n.to_string_lossy().into_owned());
    let place = root().ok_or("There's no folder for worktrees")?.join(format!("{repo_name}-{:08x}", short_hash(&repo.to_string_lossy())));
    std::fs::create_dir_all(&place).map_err(|e| format!("Couldn't make {}: {e}", place.display()))?;
    let checkout = place.join(name);
    let branch = format!("kuana/{name}");
    crate::git::run(&repo, &["worktree", "add", "-b", &branch, &checkout.to_string_lossy(), &base])?;
    let checkout = crate::paths::canonicalize(&checkout).unwrap_or(checkout);
    let rel = project.strip_prefix(&repo).unwrap_or(Path::new(""));
    Ok(Worktree { path: checkout.join(rel), checkout, branch, base, project: project.to_path_buf(), repo, removed: false })
}

/// Whether the worktree has changes not committed (untracked files too).
pub fn has_changes(worktree: &Worktree) -> bool {
    crate::git::run(&worktree.checkout, &["status", "--porcelain"]).is_ok_and(|out| !out.trim().is_empty())
}

/// Commits everything in the worktree on its branch: false when there was
/// nothing to commit.
pub fn commit_all(worktree: &Worktree, message: &str) -> Result<bool, String> {
    if !has_changes(worktree) {
        return Ok(false);
    }
    crate::git::run(&worktree.checkout, &["add", "-A"])?;
    crate::git::run(&worktree.checkout, &["commit", "-q", "-m", message])?;
    Ok(true)
}

/// Whether the branch has commits the repository's current branch lacks.
pub fn unmerged(worktree: &Worktree) -> bool {
    crate::git::run(&worktree.repo, &["rev-parse", "--verify", &worktree.branch]).is_ok() && !crate::git::is_merged(&worktree.repo, &worktree.branch)
}

/// Removes the worktree (what it holds goes) and, with `branch`, its branch.
pub fn remove(worktree: &Worktree, branch: bool) -> Result<(), String> {
    if worktree.checkout.exists() {
        crate::git::run(&worktree.repo, &["worktree", "remove", "--force", &worktree.checkout.to_string_lossy()])?;
    } else {
        // Deleted by hand: Git forgets it.
        crate::git::run(&worktree.repo, &["worktree", "prune"])?;
    }
    if branch {
        crate::git::run(&worktree.repo, &["branch", "-D", &worktree.branch])?;
    }
    // The repository's folder of worktrees, when that was its last.
    if let Some(place) = worktree.checkout.parent() {
        let _ = std::fs::remove_dir(place);
    }
    Ok(())
}

/// FNV-1a: a stable name for a repository's folder of worktrees.
fn short_hash(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5u32, |hash, byte| (hash ^ byte as u32).wrapping_mul(0x0100_0193))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn a_worktree_is_made_committed_merged_and_removed() {
        let repo = std::env::temp_dir().join(format!("kuana-worktree-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join("app")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("app/main.py"), "print('one')\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "start"]);
        // Uncommitted in the project: not in the worktree.
        std::fs::write(repo.join("app/scratch.txt"), "mine").unwrap();
        let repo = crate::paths::canonicalize(&repo).unwrap();
        let project = repo.join("app");

        let wt = create(&project, &name_for("1764100000-1a2b3c4d")).unwrap();
        assert!(!wt.removed);
        assert_eq!(wt.branch, "kuana/agent-2b3c4d");
        assert_eq!(wt.path, wt.checkout.join("app"));
        assert!(wt.path.join("main.py").exists());
        assert!(!wt.path.join("scratch.txt").exists());
        assert!(!wt.checkout.starts_with(&repo), "kept outside the project");
        assert!(!has_changes(&wt) && !unmerged(&wt));

        // The agent's edit, committed on the branch, then merged.
        std::fs::write(wt.path.join("main.py"), "print('two')\n").unwrap();
        assert!(has_changes(&wt));
        assert!(commit_all(&wt, "Say two").unwrap());
        assert!(!commit_all(&wt, "Again").unwrap(), "nothing left to commit");
        assert!(unmerged(&wt));
        git(&repo, &["merge", "-q", "--no-edit", &wt.branch]);
        assert!(!unmerged(&wt));
        assert_eq!(std::fs::read_to_string(project.join("main.py")).unwrap(), "print('two')\n");

        remove(&wt, true).unwrap();
        assert!(!wt.checkout.exists());
        assert!(crate::git::run(&repo, &["rev-parse", "--verify", &wt.branch]).is_err(), "the branch went too");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn repository_paths_map_into_the_checkout() {
        let wt = Worktree {
            path: "/w/agent-1/app".into(),
            checkout: "/w/agent-1".into(),
            branch: "kuana/agent-1".into(),
            base: "abc".into(),
            project: "/r/app".into(),
            repo: "/r".into(),
            removed: false,
        };
        assert_eq!(wt.map(Path::new("/r/app/main.py")), PathBuf::from("/w/agent-1/app/main.py"));
        assert_eq!(wt.map(Path::new("/elsewhere/x")), PathBuf::from("/elsewhere/x"));
    }

    #[test]
    fn names_take_the_session_ids_end() {
        assert_eq!(name_for("1764100000-1a2b3c4d"), "agent-2b3c4d");
        assert_eq!(name_for("x"), "agent-x");
    }
}
