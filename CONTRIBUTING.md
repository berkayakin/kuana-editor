# Contributing to Kuana

Thanks for helping! Kuana is young, so every bug report, theme, doc fix and feature makes a real
difference. This page covers how to get started, the checks a change has to pass, and the
conventions the code follows.

## Ways to help

- **Report bugs.** Open an [issue](https://github.com/berkayakin/kuana-editor/issues/new/choose) with
  the version from **About Kuana → Copy Version**. If Kuana crashed, attach the newest
  `panic-<time>.log` from the `logs` folder in its config folder (`~/.config/kuana`,
  `%APPDATA%\kuana` on Windows). Reports from Windows and Linux are especially
  valuable: those builds have had the least testing.
- **Make a theme.** A theme is a JSON file with only the colors it changes; see
  [Themes](docs/themes.md). Built-in themes live in [`assets/themes/`](assets/themes).
- **Make or test an extension.** Kuana reads Zed's extension format; see
  [Extensions](docs/extensions.md). `kuana extension check <folder>` validates one.
- **Improve the docs** in [`docs/`](docs): if something confused you, it'll confuse the next person.
- **Write code.** Look for issues labeled
  [`good first issue`](https://github.com/berkayakin/kuana-editor/labels/good%20first%20issue), or
  pick something from the roadmap below. For anything larger than a bug fix, please open an issue
  first so we can agree on the approach before you put in the time.

## Building

You need Rust 1.94 or newer and `cmake` (for wasmtime): `brew install cmake` on macOS. On Linux,
install GPUI's dependencies (the list is in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)).

```bash
cargo build                 # debug build: dependencies optimized, Kuana itself rebuilds quickly
cargo run --release -- <folder> [<file>…]
cargo test
cargo clippy --all-targets
```

The first build compiles GPUI, tree-sitter grammars and rumdl, which takes several minutes;
after that, incremental builds take seconds.

To try a change without touching your own settings, point Kuana at a scratch config folder:

```bash
XDG_CONFIG_HOME=/tmp/kuana-test ./target/release/kuana <folder>
```

## Before you open a pull request

- `cargo test` passes.
- `cargo clippy --all-targets` has **no warnings**: CI builds with `-D warnings` on Linux, then
  runs the tests on macOS and Windows.
- User-facing changes are documented in [`docs/`](docs) (and the README if it's a headline
  feature). A new setting gets a row in [`docs/settings.md`](docs/settings.md).
- Prefer unit tests for logic; see the tests in `src/lint/`, `src/search.rs`, `src/git.rs` (a
  real repository in a temp folder) and `src/document.rs`.
- Commit titles are a short sentence describing the result, like the existing history:
  "Overlay scrollbars for every scrolling list", "Code lenses above lines".

## How the code is organized

Start with [Architecture](docs/architecture.md): Kuana is one Rust crate on
[GPUI](https://www.gpui.rs). `Workspace` is the window's root view, and each open file is an
`Editor` showing a `Document` (a `ropey` rope, undo history, syntax tree and diagnostics).

## Conventions

These keep Kuana consistent; reviews will point back to them.

- **Offsets are UTF-8 byte offsets** everywhere (rope, tree-sitter, diagnostics). Convert to
  UTF-16 only at the IME and language-server boundaries, and to chars only for display.
- **Background work:** spawn heavy work with `cx.background_executor()`. When it finishes, check
  that the document's `version()` (or a request id) hasn't changed before applying the result.
- **Text fields are editors** (`Editor::single_line` / `multi_line`), not custom widgets.
- **Colors:** never hard-code one; use `Theme::get(cx)`. A new color needs a field in `Theme`, an
  entry in `color_keys!`, a value in **every** `assets/themes/*.json` (a test checks this) and a
  row in [`docs/themes.md`](docs/themes.md).
- **Key bindings** live only in `src/keymap.rs`, with a shortcut in each base keymap (JetBrains,
  VS Code, Sublime Text, Zed) that has one; a test checks for conflicts. New user-facing actions go
  into `action_entries()` so Find Action lists them. Shortcuts shown in the UI come from
  `ui::binding_label`, never hard-coded strings.
- **Settings:** a field in `Settings`/`EditorSettings`, its name in `TOP_LEVEL_KEYS`/`EDITOR_KEYS`,
  a commented line in `SETTINGS_TEMPLATE`, a `Setting` variant in `preferences.rs` and a row in
  [`docs/settings.md`](docs/settings.md). Editors read per-file options in `apply_settings`. If a
  project may set it in `.kuana/settings.json`, add it to `project_settings::PROJECT_KEYS` and read
  it through `Settings::at(path)`.
- **UI:** use the helpers in `src/ui.rs` (`island`, `popover`, `modal_panel`, `soft_shadow`,
  `focus_ring`, `hover_bg`/`row_hover`…) instead of ad-hoc borders, shadows or hover styles, so the
  islands look and the **Visual effects** setting (Full / Reduced / Minimal) apply everywhere.
  Animate state changes with `with_transition` (`src/transition.rs`), not timers.
- **Icons** are Google's [Material Symbols](https://fonts.google.com/icons) (Rounded, filled,
  24 px), unchanged; add one to `assets/icons/` and `src/assets.rs`.
- **Git** always runs through `git::run`. Destructive actions ask for confirmation, and deletes go
  to the Trash.
- **Languages:** prefer adding one as an [extension](docs/extensions.md) over compiling it in.
- **Comments** explain *why*, not what; module docs use `//!`.

## Roadmap

Things we'd love help with:

- Git: a three-way merge editor, interactive rebase
- Vim: registers, macros, marks, `:s`
- Language servers: semantic tokens, choosing and ordering servers per language
- Extensions: debug adapters and completion labels from extensions' code, Zed's older extension
  APIs, more languages in the registry
- Debugger: hit counts, exception breakpoint options
- Windows and Linux polish

## License

By contributing, you agree that your contributions are dual licensed under the
[MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE) licenses, like the rest of Kuana.
