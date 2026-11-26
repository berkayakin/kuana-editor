//! Keeps project-side code on the project's host (`crate::fs`, `crate::exec`;
//! see `plans/remote-dev.md`): these modules' own code (their tests aside)
//! mustn't use `std::fs` or start programs with `std::process`, or a remote
//! project would quietly read, write or run on this computer.

/// Modules that work with the project's files and tools.
const PROJECT_SIDE: &[&str] = &[
    "document.rs",
    "project_tree.rs",
    "workspace.rs",
    "workspace/menus.rs",
    "workspace/file_tree.rs",
    "workspace/commands.rs",
    "workspace/saving.rs",
    "workspace/watching.rs",
    "workspace/todo.rs",
    "workspace/test_explorer.rs",
    "workspace/compare.rs",
    "workspace/pane.rs",
    "workspace/navigation.rs",
    "workspace/nav_history.rs",
    "workspace/opening.rs",
    "workspace/bookmarks.rs",
    "workspace/language_servers.rs",
    "workspace/line_endings.rs",
    "workspace/sessions.rs",
    "workspace/git_ops.rs",
    "workspace/terminals.rs",
    "run.rs",
    "diff_view.rs",
    "preview.rs",
    "image_view.rs",
    "markdown_preview.rs",
    "python.rs",
    "project_settings.rs",
    "editorconfig.rs",
    "todo.rs",
    "test_explorer.rs",
    "format.rs",
    "lint/external.rs",
    "test_run.rs",
    "git.rs",
    "git_store.rs",
    "file_history.rs",
    "merge_view.rs",
    "editor.rs",
    "editor/lsp.rs",
];

/// Lines that are about this computer on purpose.
const LOCAL: &[(&str, &str)] = &[
    // The pty's own handle.
    ("terminal.rs", "std::fs::File"),
];

#[test]
fn project_side_code_goes_through_the_host() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for file in PROJECT_SIDE {
        let text = std::fs::read_to_string(src.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        // A Windows checkout may have CRLF line endings.
        let text = text.replace("\r\n", "\n");
        let own = text.split("#[cfg(test)]\nmod ").next().unwrap_or("");
        for (n, line) in own.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            let bad = code.contains("std::fs::") || code.contains("std::process::Command") || code.contains("crate::process::command(");
            if bad && !LOCAL.iter().any(|(f, what)| f == file && code.contains(what)) {
                found.push(format!("{file}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "use crate::fs / crate::exec in project-side code:\n{}", found.join("\n"));
}
