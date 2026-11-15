//! The folders a window's project is made of (see `plans/multi-root.md`):
//! an ordered list, the first being the **primary** one, which names the
//! window and is where project-wide things start when nothing else says. No
//! folder is inside another, so every path belongs to at most one. A single
//! folder, the usual case, behaves as projects always have.

use std::path::{Path, PathBuf};

use kuana_core::remote::connection::host_of;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    folders: Vec<PathBuf>,
}

/// Why a folder can't be added.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AddError {
    /// It's in the project already.
    Duplicate,
    /// It's inside one of the project's folders, or holds one.
    Nested(PathBuf),
    /// It's on another machine than the project (a remote mount, or this computer).
    OtherHost,
}

impl Project {
    /// A project of one folder.
    pub fn new(folder: PathBuf) -> Self {
        Project { folders: vec![folder] }
    }

    /// The first folder: the window's name, its session, where a new
    /// terminal starts without a file to go by.
    pub fn primary(&self) -> &Path {
        &self.folders[0]
    }

    pub fn folders(&self) -> &[PathBuf] {
        &self.folders
    }

    /// The folder holding `path` (or being it).
    pub fn folder_of(&self, path: &Path) -> Option<&Path> {
        // `Path::starts_with` compares whole components: /a/bc isn't in /a/b.
        self.folders.iter().find(|f| path.starts_with(f)).map(PathBuf::as_path)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.folder_of(path).is_some()
    }

    /// Whether `path` is one of the folders themselves.
    pub fn is_folder(&self, path: &Path) -> bool {
        self.folders.iter().any(|f| f == path)
    }

    /// `path` as the project shows it: relative to its folder, with the
    /// folder's name in front once there are several (`web/src/app.ts`), and
    /// None outside the project.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let folder = self.folder_of(path)?;
        let rel = path.strip_prefix(folder).ok()?;
        if self.folders.len() == 1 {
            return Some(rel.display().to_string());
        }
        let name = folder_name(folder);
        if rel.as_os_str().is_empty() {
            return Some(name);
        }
        Some(Path::new(&name).join(rel).display().to_string())
    }

    /// The project's name: its folder's, or several joined ("api, web").
    pub fn name(&self) -> String {
        self.folders.iter().map(|f| folder_name(f)).collect::<Vec<_>>().join(", ")
    }

    /// Adds a folder after the others.
    #[allow(dead_code)] // Add Folder to Project (phase 1)
    pub fn add(&mut self, folder: PathBuf) -> Result<(), AddError> {
        if self.is_folder(&folder) {
            return Err(AddError::Duplicate);
        }
        if let Some(other) = self.folders.iter().find(|f| folder.starts_with(f) || f.starts_with(&folder)) {
            return Err(AddError::Nested(other.clone()));
        }
        // By the path, so it holds while a connection is down too.
        if host_of(&folder) != host_of(self.primary()) {
            return Err(AddError::OtherHost);
        }
        self.folders.push(folder);
        Ok(())
    }

    /// Removes a folder; never the last one. False when it wasn't there or
    /// was the only one.
    #[allow(dead_code)] // Remove Folder from Project (phase 1)
    pub fn remove(&mut self, folder: &Path) -> bool {
        if self.folders.len() == 1 {
            return false;
        }
        let before = self.folders.len();
        self.folders.retain(|f| f != folder);
        self.folders.len() != before
    }
}

/// A folder's last component (the whole path for a root like `/`).
pub fn folder_name(folder: &Path) -> String {
    folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| folder.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_belong_to_one_folder() {
        let mut project = Project::new("/w/api".into());
        assert_eq!(project.folder_of(Path::new("/w/api/src/main.rs")), Some(Path::new("/w/api")));
        assert_eq!(project.folder_of(Path::new("/w/api")), Some(Path::new("/w/api")));
        // Whole components: /w/apis isn't in /w/api.
        assert_eq!(project.folder_of(Path::new("/w/apis/x.rs")), None);
        assert!(!project.contains(Path::new("/w/web/a.ts")));
        // One folder: labels are as they always were.
        assert_eq!(project.relative(Path::new("/w/api/src/main.rs")).as_deref(), Some("src/main.rs"));
        assert_eq!(project.relative(Path::new("/w/api")).as_deref(), Some(""));
        assert_eq!(project.name(), "api");

        assert_eq!(project.add("/w/web".into()), Ok(()));
        assert_eq!(project.primary(), Path::new("/w/api"));
        assert!(project.contains(Path::new("/w/web/a.ts")));
        assert_eq!(project.relative(Path::new("/w/web/src/a.ts")).as_deref(), Some("web/src/a.ts"));
        assert_eq!(project.relative(Path::new("/w/web")).as_deref(), Some("web"));
        assert_eq!(project.relative(Path::new("/elsewhere/a.ts")), None);
        assert_eq!(project.name(), "api, web");
        assert!(project.is_folder(Path::new("/w/web")) && !project.is_folder(Path::new("/w/web/src")));
    }

    #[test]
    fn folders_are_never_nested_or_doubled() {
        let mut project = Project::new("/w/api".into());
        assert_eq!(project.add("/w/api".into()), Err(AddError::Duplicate));
        assert_eq!(project.add("/w/api/vendor".into()), Err(AddError::Nested("/w/api".into())));
        assert_eq!(project.add("/w".into()), Err(AddError::Nested("/w/api".into())));
        let remote = crate::fs::mount_root("box").join("home/me/web");
        assert_eq!(project.add(remote), Err(AddError::OtherHost));
        // The last folder stays.
        assert!(!project.remove(Path::new("/w/api")));
        project.add("/w/web".into()).unwrap();
        assert!(!project.remove(Path::new("/w/other")));
        assert!(project.remove(Path::new("/w/api")));
        assert_eq!(project.primary(), Path::new("/w/web"));
    }
}
