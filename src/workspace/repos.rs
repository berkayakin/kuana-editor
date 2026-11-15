//! Git across the project's folders (`plans/multi-root.md`, phase 2): each
//! folder has a `GitStore` of its own, which finds the repository it's in.
//! One of them is shown (`Workspace::git`): the Commit view, the branch, the
//! log and Git operations are its. It follows the active file's repository,
//! and the Commit view's repository picker switches it by hand. What covers
//! files anywhere (the tree's colors, editors' changed lines, status read
//! after changes on disk, background fetches) goes to each file's own
//! repository. Two folders in one repository share its status: the first
//! folder's store stands for it.

use super::*;

/// A project folder's Git store.
pub(super) struct FolderRepo {
    pub(super) folder: PathBuf,
    pub(super) store: Entity<GitStore>,
    /// The primary folder's store is subscribed to for as long as the
    /// window lives (`primary_git`); the others for as long as their folder
    /// is in the project.
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Gives every project folder a store (the primary's is `primary_git`),
    /// drops removed folders' and has the new ones find their repository.
    /// The shown store stays shown while its folder is in the project.
    pub(super) fn sync_repos(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folders = self.folders.as_ref().map(|p| p.folders().to_vec()).unwrap_or_default();
        let Some(primary) = folders.first().cloned() else {
            self.repos.clear();
            let primary = self.primary_git.clone();
            self.show_repo(primary, cx);
            return;
        };
        let before: Vec<PathBuf> = self.repos.iter().map(|r| r.folder.clone()).collect();
        self.repos.retain(|r| folders.contains(&r.folder) && r.folder != primary);
        let mut repos = Vec::new();
        for (ix, folder) in folders.iter().enumerate() {
            let existing = self.repos.iter().position(|r| &r.folder == folder);
            let repo = match (ix, existing) {
                (0, _) => FolderRepo { folder: folder.clone(), store: self.primary_git.clone(), _subscriptions: Vec::new() },
                (_, Some(at)) => self.repos.remove(at),
                (_, None) => {
                    let store = cx.new(|_| GitStore::new());
                    let subscriptions = vec![cx.subscribe_in(&store, window, Self::on_git_event), cx.observe(&store, |_, _, cx| cx.notify())];
                    FolderRepo { folder: folder.clone(), store, _subscriptions: subscriptions }
                }
            };
            // The primary's is found by `detect_repo` (it also looks again when its folder comes back).
            if ix > 0 && !before.contains(folder) {
                let (dir, root) = (folder.clone(), Some(folder.clone()));
                repo.store.update(cx, |g, cx| g.detect(dir, root, cx));
            }
            repos.push(repo);
        }
        self.repos = repos;
        // The shown store's folder went: the primary's is shown.
        if !self.repos.iter().any(|r| r.store == self.git) {
            let primary = self.primary_git.clone();
            self.show_repo(primary, cx);
        }
    }

    /// The stores standing for the project's distinct repositories, in
    /// folder order, with each one's repository (folders outside any
    /// repository left out).
    pub(super) fn repositories(&self, cx: &App) -> Vec<(Entity<GitStore>, PathBuf)> {
        let mut out: Vec<(Entity<GitStore>, PathBuf)> = Vec::new();
        for repo in &self.repos {
            let Some(root) = repo.store.read(cx).repo().cloned() else { continue };
            if !out.iter().any(|(_, r)| *r == root) {
                out.push((repo.store.clone(), root));
            }
        }
        out
    }

    /// The store of the repository holding `path` (the innermost one: a
    /// folder's repository can sit inside another folder's).
    pub(super) fn store_for(&self, path: &Path, cx: &App) -> Option<Entity<GitStore>> {
        innermost(path, &self.repositories(cx)).cloned()
    }

    /// Shows another repository: the Commit view, branch and log are its.
    pub(super) fn show_repo(&mut self, store: Entity<GitStore>, cx: &mut Context<Self>) {
        if store == self.git {
            return;
        }
        self.git = store;
        self.repo_changed(cx);
        let status = self.git_status(cx);
        self.git_panel.update(cx, |p, cx| p.set_status(status, cx));
        // Its stashes come with a whole read.
        self.git.update(cx, |g, cx| g.refresh(cx));
        cx.notify();
    }

    /// The active file moved: the repository it's in is shown, when it has one.
    pub(super) fn follow_active_repo(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_item(cx).and_then(|t| t.path(cx)) else { return };
        if let Some(store) = self.store_for(&path, cx) {
            self.show_repo(store, cx);
        }
    }

    /// The Commit view's repository picker: each repository, by its folder's
    /// name and branch, the shown one checked.
    pub(super) fn open_repository_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        for (store, root) in self.repositories(cx) {
            let branch = store.read(cx).status().branch.clone().unwrap_or_default();
            let name = crate::project_folders::folder_name(&root);
            let label = if branch.is_empty() { name } else { format!("{name}  ·  {branch}") };
            let icon = if store == self.git { Icon::Check } else { Icon::GitBranch };
            entries.push(MenuEntry::item(label, icon));
            actions.push(Some(MenuAction::ShowRepository(store)));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Every store's tree data together: changed and ignored paths of all
    /// the project's repositories. Merged once per change of any of them
    /// (`merged_git`): it's asked for on every frame, and the tree redraws
    /// only for new maps.
    pub(super) fn all_git_paths(&self, cx: &App) -> (Arc<HashMap<PathBuf, git::Change>>, Arc<HashSet<PathBuf>>) {
        let repos = self.repositories(cx);
        if let [(store, _)] = repos.as_slice() {
            let git = store.read(cx);
            return (git.paths().clone(), git.ignored().clone());
        }
        let parts: Vec<(Arc<HashMap<PathBuf, git::Change>>, Arc<HashSet<PathBuf>>)> =
            repos.iter().map(|(store, _)| (store.read(cx).paths().clone(), store.read(cx).ignored().clone())).collect();
        let key: Vec<(usize, usize)> = parts.iter().map(|(p, i)| (Arc::as_ptr(p) as *const () as usize, Arc::as_ptr(i) as *const () as usize)).collect();
        if let Some((cached, paths, ignored)) = self.merged_git.borrow().as_ref()
            && *cached == key
        {
            return (paths.clone(), ignored.clone());
        }
        let mut paths = HashMap::new();
        let mut ignored = HashSet::new();
        for (p, i) in &parts {
            paths.extend(p.iter().map(|(path, change)| (path.clone(), *change)));
            ignored.extend(i.iter().cloned());
        }
        let merged = (Arc::new(paths), Arc::new(ignored));
        *self.merged_git.borrow_mut() = Some((key, merged.0.clone(), merged.1.clone()));
        merged
    }
}

/// Of `repos` (each with its working tree's root), the one holding `path`
/// deepest: a repository nested in another (a submodule, a folder's own
/// repository inside another folder's) owns its files.
fn innermost<'a, T>(path: &Path, repos: &'a [(T, PathBuf)]) -> Option<&'a T> {
    repos.iter().filter(|(_, root)| path.starts_with(root)).max_by_key(|(_, root)| root.components().count()).map(|(repo, _)| repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_belongs_to_the_innermost_repository() {
        let repos = [("outer", PathBuf::from("/w")), ("api", PathBuf::from("/w/api")), ("web", PathBuf::from("/x/web"))];
        assert_eq!(innermost(Path::new("/w/api/src/main.rs"), &repos), Some(&"api"));
        assert_eq!(innermost(Path::new("/w/docs/a.md"), &repos), Some(&"outer"));
        assert_eq!(innermost(Path::new("/x/web/app.ts"), &repos), Some(&"web"));
        // Whole components: /w/apis is the outer repository's.
        assert_eq!(innermost(Path::new("/w/apis/a.rs"), &repos), Some(&"outer"));
        assert_eq!(innermost(Path::new("/elsewhere/a.rs"), &repos), None);
    }
}
