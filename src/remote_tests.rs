//! The app's own project-side code against a remote host, minus SSH: a
//! `kuana-server` on a thread, connected over pipes and mounted (as
//! `kuana_core::remote`'s tests do), then Git, documents and the file tree
//! on a repository the "remote" side has.

use std::path::{Path, PathBuf};

use gpui::{AppContext, Entity, TestAppContext};
use kuana_core::remote::{RemoteHost, client, server};

use crate::{
    document::Document,
    editor::Editor,
    fs::PathExt,
    lsp::{LspStore, fake_server},
};

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

/// A remote project's language server runs there, is told the host's own
/// paths, and its answers reach Kuana's documents under the mount.
#[gpui::test]
fn language_servers_run_on_the_host(cx: &mut TestAppContext) {
    if crate::lsp::servers::which("python3").is_none() {
        return;
    }
    cx.executor().allow_parking();
    crate::trust::set_trust_all(true);
    let dir = temp("lsp");
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let (root, host) = connect("app-remote-lsp");
    let project = mounted(&root, &dir);
    let store = cx.new(|_| {
        let mut store = LspStore::new();
        store.test_specs = Some(vec![fake_server("uri", "")]);
        store.set_root(Some(project.clone()));
        store
    });
    let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&project.join("a.rs")).unwrap(), cx));
    store.update(cx, |s, cx| s.register(&editor, cx));
    let started = std::time::Instant::now();
    let message = loop {
        cx.run_until_parked();
        let found = editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.first().map(|d| d.message.to_string()));
        if let Some(message) = found {
            break message;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "no diagnostics came");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // The server read `file://<the host's path>`, not the mount's.
    let host_uri = url::Url::from_file_path(dir.join("a.rs")).unwrap().to_string();
    assert!(message.ends_with(&format!("saw {host_uri}")), "{message}");
    assert!(!message.contains("kuana-remote"), "{message}");
    host.disconnect();
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run targets for a remote project: rust-analyzer's places (the host's)
/// come back under the mount, and a lone C file builds into the host's /tmp.
#[test]
fn run_targets_point_at_the_host() {
    let dir = temp("run");
    std::fs::write(dir.join("hello.c"), "int main(void) { return 0; }\n").unwrap();
    let (root, host) = connect("app-remote-run");
    let project = mounted(&root, &dir);
    let runnable = serde_json::json!({
        "label": "run demo",
        "args": { "workspaceRoot": dir.to_string_lossy(), "cargoArgs": ["run", "--bin", "demo"], "executableArgs": [] },
    });
    let target = crate::run::from_rust_analyzer(&runnable, &project.join("src/main.rs")).unwrap();
    assert_eq!(target.cwd, project);
    let plan = crate::run::native_plan(&project.join("hello.c"), false).unwrap();
    assert_eq!(plan.program, crate::fs::local_path(&root, "/tmp/kuana-run-hello"));
    assert_eq!(crate::fs::host_text(&root, &plan.build[0].join(" ")), format!("cc -g -O0 {}/hello.c -o /tmp/kuana-run-hello", dir.display()));
    host.disconnect();
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A dropped connection takes its language servers along; once the
/// machine is back, they start again over the new one.
#[gpui::test]
fn language_servers_start_again_after_a_reconnect(cx: &mut TestAppContext) {
    if crate::lsp::servers::which("python3").is_none() {
        return;
    }
    cx.executor().allow_parking();
    crate::trust::set_trust_all(true);
    let dir = temp("lsp-again");
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let (root, host) = connect("app-remote-lsp-again");
    let project = mounted(&root, &dir);
    let store = cx.new(|_| {
        let mut store = LspStore::new();
        store.test_specs = Some(vec![fake_server("uri", "")]);
        store.set_root(Some(project.clone()));
        store
    });
    let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&project.join("a.rs")).unwrap(), cx));
    store.update(cx, |s, cx| s.register(&editor, cx));
    let diagnosed = |cx: &mut TestAppContext| {
        let started = std::time::Instant::now();
        loop {
            cx.run_until_parked();
            if editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()) {
                return;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(20), "no diagnostics came");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    diagnosed(cx);
    let first = store.read_with(cx, |s, _| s.running_servers());
    assert_eq!(first.len(), 1);

    // The connection goes; another server is mounted in its place.
    host.disconnect();
    let (again, host) = connect("app-remote-lsp-again");
    assert_eq!(again, root);
    store.update(cx, |s, cx| s.restart_under(&root, cx));
    cx.run_until_parked();
    diagnosed(cx);
    let second = store.read_with(cx, |s, _| s.running_servers());
    assert_eq!(second.len(), 1);
    assert!(!std::sync::Arc::ptr_eq(&first[0], &second[0]), "the server started again");
    host.disconnect();
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Attach to Process on a remote project lists the host's processes, with
/// their programs under the mount (the adapter is told the host's paths).
#[test]
fn processes_to_attach_to_are_the_hosts() {
    let dir = temp("attach");
    let (root, host) = connect("app-remote-attach");
    let project = mounted(&root, &dir);
    let mut sleeper = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let listed = crate::debugger::launch::processes(&project);
    let found = listed.iter().find(|p| p.pid == sleeper.id());
    let _ = sleeper.kill();
    let found = found.unwrap_or_else(|| panic!("sleep isn't listed among {}", listed.len()));
    assert!(found.program.starts_with(&root), "{}", found.program.display());
    assert!(found.name.contains("sleep"), "{}", found.name);
    host.disconnect();
    client::unmount(&root);
    let _ = std::fs::remove_dir_all(&dir);
}
