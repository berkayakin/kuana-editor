# Architecture

Kuana is one Rust binary crate built on [GPUI](https://www.gpui.rs). The window's root view is
`Workspace`; every open file is an `Editor` showing a `Document` (a `ropey` rope with undo
history, a tree-sitter syntax tree and diagnostics). The editor is a custom GPUI element that
shapes and paints only the lines on screen; parsing, linting, Git, search and language servers
run off the main thread.

The conventions to follow when changing the code (offsets, background work, settings, key
bindings, colors, icons) are in the [contributing guide](../CONTRIBUTING.md).

- `src/main.rs` — app setup: fonts, theme, keymap, menus, windows, command-line paths
- `src/workspace.rs` — window chrome: title bar, file tree, panes and tabs, problems, status bar,
  modals and file operations
- `src/document.rs` — rope buffer, edits, undo history, file I/O
- `src/editor.rs` — editor view, input handling and the GPU element that paints it
- `src/wrap.rs` — soft wrap: where long lines break into display rows
- `src/fold.rs`, `src/editor/fold.rs` — code folding regions and fold UI
- `src/syntax.rs` — tree-sitter parsing, highlighting, bracket matching and file outlines
- `src/lsp/` — language server client: JSON-RPC transport, protocol types, server discovery
- `src/editor/lsp.rs` — completion, hover, signature help and LSP requests in the editor
- `src/lsp/copilot.rs`, `src/editor/prediction.rs`, `src/workspace/copilot.rs` — edit predictions
  from GitHub Copilot: its language server beside a file's own, sign-in, the ghost text and the
  status bar item
- `src/results_panel.rs` — the Find panel: results as trees in tabs (usages, hierarchies, test
  runs, Local History), with children that can load as they open
- `src/hierarchy.rs` — Call and Type Hierarchy from the language server, for the Find panel
- `src/extensions.rs` — extensions in Zed's format: languages, grammars, themes, snippets, servers
- `src/extensions/wasm.rs` — runs extensions' own code (`extension.wasm`, Zed's extension API
  0.6/0.7) for their language servers, sandboxed; `src/extensions/wit/` is that API's interface
- `src/extensions/registry.rs` — Kuana's extension registry: the signed list, install and update
- `src/format.rs`, `src/editor/format.rs` — Reformat Code and format on save
- `src/lint/` — built-in JS/HTML/Markdown linters, tree-sitter syntax errors and the ESLint bridge
- `src/search.rs`, `src/find_bar.rs`, `src/project_search.rs` — in-file and project search
- `src/picker.rs` — Search Everywhere, Go to File, Recent Files, Recent Projects, Find Action,
  File Structure, Go to Line
- `src/terminal.rs` — the integrated terminal
- `src/run.rs`, `src/editor/run.rs` — run and test targets and the gutter ▶
- `src/test_run.rs` — test results read from a run's output (cargo, go, pytest, Jest, Vitest,
  node, `oxie test`), their tree, the gutter's ✓ / ✕ and Rerun Failed
- `src/local_history.rs`, `src/workspace/local_history.rs` — Local History: files' earlier
  versions under `<config>/local_history/`, listed with a diff and Restore
- `src/keymap.rs` — JetBrains-style key bindings
- `src/git.rs`, `src/git_panel.rs`, `src/diff_view.rs`, `src/git_log.rs`, `src/push_dialog.rs`,
  `src/branch_picker.rs`, `src/clone_dialog.rs` — Git backend and UI
- `src/conflict.rs`, `src/editor/conflict.rs` — merge conflict markers and their resolution
- `src/workspace/pane.rs`, `src/workspace/pane_tree.rs` — a pane (its tabs and their strip) and
  the tree of rows and columns the panes divide the editor area into
- `src/pane_item.rs` — what a pane's tab can show: an editor, an image or a Markdown preview
- `src/preview.rs`, `src/image_view.rs` — image files and their tab
- `src/markdown_preview.rs`, `src/markdown_preview/view.rs` — the Markdown preview and its tab
- `src/settings.rs`, `src/editorconfig.rs`, `src/preferences.rs`, `src/preferences/` —
  `settings.json`, `.editorconfig` and the Settings window
- `src/project_settings.rs` — a project's `.kuana/settings.json`, layered over the user's
- `src/state.rs` — remembered UI state and per-project sessions
- `src/theme.rs`, `src/theme_picker.rs`, `assets/themes/` — theme loading, picker and built-in themes
- `src/updater.rs`, `.github/workflows/release.yml` — auto-update and the release build
- `src/about.rs` — the About Kuana window
- `src/panic_log.rs` — a crash's message, place and backtrace in `<config>/logs/panic-<time>.log`;
  the next start offers to report it as a GitHub issue filled in from the log
- `src/autopilot.rs` — plays a script of keys and actions (`KUANA_AUTOPILOT=<file>`): the README
  recording (`scripts/record.sh`) and CI's smoke test (`scripts/smoke.sh`, `.github/workflows/smoke.yml`)
- `src/ui.rs`, `src/context_menu.rs`, `src/input_dialog.rs` — shared widgets, menus and prompts
- `src/assets.rs`, `assets/icons/` — UI icons ([Material Symbols](https://fonts.google.com/icons), Rounded, filled; Apache 2.0) and the title font embedded into the binary
- `assets/file-icons/`, `src/file_icons.rs` — file-type icons ([Charmed Icons](https://github.com/jmesrje/zed-charmed-icons)) and their file-name associations
- `assets/app-icon/` — the app logo (`logo.png`) and its per-platform icons
