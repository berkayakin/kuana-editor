//! The app's own project-side code against a remote host, minus SSH: a
//! `kuana-server` on a thread, connected over pipes and mounted (as
//! `kuana_core::remote`'s tests do), then Git, documents and the file tree
//! on a repository the "remote" side has.

use std::path::{Path, PathBuf};

use kuana_core::remote::{RemoteHost, client, server};

use crate::{document::Document, fs::PathExt};

/// A server on a thread, mounted as `name`.
fn connect(name: &str) -> (PathBuf, std::sync::Arc<RemoteHost>) {
    let (server_in, client_out) = std::io::pipe().unwrap();
    let (client_in, server_out) = std::io::pipe().unwrap();
    std::thread::spawn(move || server::serve(server_in, server_out, Default::default()));
    let host = RemoteHost::start(client_in, client_out);
    (client::mount(name, host.clone()), host)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuana-app-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn mounted(root: &Path, real: &Path) -> PathBuf {
    crate::fs::local_path(root, &real.to_string_lossy())
}

#[test]
fn git_documents_and_the_tree_work_on_a_remote_project() {
    if !crate::git::is_available() {
        return;
    }
    let dir = temp("project");
    let (root, host) = connect("app-remote-test");
    let project = mounted(&root, &dir);

    // A repository made there, through Kuana's Git.
    crate::git::init(&project).unwrap();
    for (k, v) in [("user.name", "Test"), ("user.email", "t@example.com")] {
        crate::git::run(&project, &["config", k, v]).unwrap();
    }
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    crate::git::run(&project, &["add", "."]).unwrap();
    crate::git::run(&project, &["commit", "-m", "first"]).unwrap();
    // Git's own absolute paths come back as mount paths.
    assert_eq!(crate::git::discover(&project.join("src")), Some(project.clone()));
    assert!(crate::git::git_dir(&project).is_some_and(|d| d.starts_with(&root) && d.is_dir_on_host()));

    // A document opened, edited and saved there.
    let file = project.join("src/main.rs");
    let mut doc = Document::open(&file).unwrap();
    assert_eq!(doc.rope().to_string(), "fn main() {}\n");
    assert!(!doc.changed_on_disk());
    doc.edit(vec![(0..0, "// remote\n".to_string())], &[], crate::document::EditKind::Other);
    doc.save().unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("src/main.rs")).unwrap(), "// remote\nfn main() {}\n");
    assert!(!doc.changed_on_disk());
    // Changed there behind its back: noticed.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.join("src/main.rs"), "changed\n").unwrap();
    assert!(doc.changed_on_disk());

    // Git status sees it, with mount paths.
    let status = crate::git::status(&project).unwrap();
    assert!(status.files.iter().any(|f| f.path == file), "{:?}", status.files.iter().map(|f| &f.path).collect::<Vec<_>>());
    assert_eq!(status.branch.as_deref().map(|b| !b.is_empty()), Some(true));

    // The tree's listing.
    std::fs::write(dir.join("README.md"), "x").unwrap();
    let names: Vec<(String, bool)> = crate::project_tree::children(&project).into_iter().map(|(_, is_dir, _, name)| (name, is_dir)).collect();
    assert_eq!(names, [("src".to_string(), true), ("README.md".to_string(), false)]);

    host.disconnect();
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}
