# Settings and project trust

## The Settings window

The first time Kuana starts, **Welcome Setup** asks for the basics: a theme (applied as you click),
the keymap to start from (each shown with a few of its shortcuts) and editor basics such as font
size, auto-save, reformat on save and soft wrap. Its choices go to `settings.json` like any other.
Open it again any time from the Kuana menu or Find Action (**Welcome Setup**).

**Settings…** (⌘, / Ctrl+Alt+S, or the gear at the top right) opens the Settings
window, the same on every platform. There's one for all your project windows, so it can stay
open beside the code while changes apply; opening it again brings it forward, and it reopens
where you left it, on the page you were on. Escape or ⌘W / Ctrl+W closes it. Its pages are Appearance (themes), Editor (font, indentation,
wrapping), Saving, Languages (per-language options, language server and formatter),
[Extensions](extensions.md), Keymap, Terminal & Run, Git, Trust and Updates. The search field looks through all of them; with it focused, ↑/↓ switch
pages. Changes apply right away. An option you changed has an undo button next to its name,
which puts the default back.

Everything is stored in `settings.json`. **Open settings.json** (at the bottom of the window and
in Find Action) opens it in the project window you used last, created with every option commented out. Edits to the file apply when
you save (or when you come back to Kuana after editing it elsewhere). Comments and trailing commas
are fine, and Kuana keeps them when it changes a value itself. With the JSON language server
installed (`vscode-langservers-extracted`), `settings.json`, `keymap.json` and your themes get
completion, documentation on hover and checks from schemas Kuana makes of its own options, actions
and theme colors.

| Setting | Default | |
| --- | --- | --- |
| `theme` | `"Ember"` | see [Themes](themes.md) |
| `base_keymap` | `"JetBrains"` | `"JetBrains"`, `"VSCode"`, `"SublimeText"` or `"Zed"`; see [Key bindings](keybindings.md) |
| `auto_save` | `"after_delay"` | `"off"`, `"after_delay"`, `"on_focus_change"`, `"on_window_change"` |
| `auto_save_delay` | `1000` | milliseconds |
| `editor_font_family` | built-in choice | any installed font |
| `editor_font_size` | `13` | the zoom actions adjust it on top (remembered between launches) |
| `editor_line_height` | `1.57` | multiple of the font size |
| `tab_size` | `4` (2 for HTML, JSON, YAML) | columns per indent and per tab |
| `hard_tabs` | `false` (`true` for Go) | indent with tabs |
| `soft_wrap` | `false` (`true` for Markdown and plain text) | |
| `trim_trailing_whitespace` | `false` | on save; lines with a caret are left alone |
| `insert_final_newline` | `false` | on save |
| `format_on_save` | `false` | reformat on Save (auto-save never reformats) |
| `rulers` | `[]` (`.editorconfig`'s `max_line_length`) | columns for vertical guides, e.g. `[80, 120]` |
| `show_whitespace` | `"selection"` | mark spaces and tabs: `"selection"`, `"boundary"` (indentation and line ends), `"trailing"`, `"all"`, `"none"` |
| `inlay_hints` | `true` | types and parameter names from the language server |
| `code_lens` | `true` | the language server's code lenses above lines ("3 references", "▶ Run \| Debug") |
| `minimap` | `false` | the document in miniature at the editor's right edge, in place of the scrollbar: code as colored blocks, the visible part as a slider, problems, search matches and Git changes along its edge |
| `minimap_width` | `100` | the minimap's width in pixels (40–240) |
| `syntax_errors` | `true` (`false` for extension languages) | parse errors in Problems when no language server covers the file. Extensions' grammars may not know all valid code (tree-sitter-sql rejects Postgres' `ON CONFLICT (…)`), so turn it on per language for the ones you trust, e.g. `"languages": {"Gleam": {"syntax_errors": true}}` |
| `languages` | | per-language overrides of the twelve above (all but `minimap_width`), by the name in the status bar |
| `vim_mode` | `false` | Vim's modes and keys; see [Vim mode](editing.md#vim-mode) |
| `icon_theme` | built-in | an extension's icon theme by name, e.g. `"Soft Charmed Icons"` |
| `visual_effects` | `"full"` | how much decoration the interface draws: `"full"`, `"reduced"` (solid colors instead of gradients) or `"minimal"` (also no shadows, glows or animations); Settings → Appearance. The older `"reduce_visual_effects": true` still works, as `"minimal"` |
| `ui_style` | `"modern"` | the window's layout: `"modern"` (rounded islands with gaps between them, browser-like tabs) or `"classic"` (square areas side by side, separated by lines, rectangular tabs); Settings → Appearance → Layout |
| `sidebar_dock` | `"left"` | which side of the editor the sidebar (Files, Commit) sits on: `"left"` or `"right"`; Settings → Appearance, Move to Right / Left on its tabs' right-click menu, or Toggle Sidebar Position |
| `project_avatar` | `true` | the round initial before the project's name in the title bar; Settings → Appearance → Layout |
| `language_servers` | | per language: `false` turns its server off, `["cmd", "args"]` replaces it |
| `lsp` | | per server, by id (Kuana's, e.g. `typescript`, or Zed's, e.g. `vtsls`, `basedpyright`): `settings` (what it asks for through `workspace/configuration`; a change is sent to it), `initialization_options` (applied when it restarts) and `binary` (`{ "path", "arguments" }`) |
| `terminal_shell` | login shell | the terminal's shell and arguments, e.g. `["/opt/homebrew/bin/fish", "--login"]` |
| `run_configurations` | | `[{ "name", "command", "cwd" }]` added to Run…; see [Run and test](run-and-debug.md#run-and-test) |
| `formatters` | | per language: `"language_server"`, a command reading stdin and writing stdout (`{file}` is the path), or `false` |
| `trust_all_projects` | `false` | run language servers, formatters and project tools in every folder without asking; see [Project trust](#project-trust) |
| `native_grammars` | `false` | compile extensions' grammars to native code with your C compiler instead of WebAssembly (only for extensions installed from a folder in a trusted folder) |
| `auto_update` | `true` | download new releases in the background; see [Releases and updates](install.md#updates) |
| `pull_strategy` | unset | how Update Project combines incoming commits with yours when both branches have new ones: `"merge"`, `"rebase"` or `"ff_only"` (stop instead); unset, Git's `pull.rebase` / `pull.ff` decide, else the first such pull asks and saves the answer; see [Git](git.md) |

```jsonc
{
  "editor_font_size": 15,
  "tab_size": 2,
  "languages": {
    "Python": { "tab_size": 4 },
    "Markdown": { "soft_wrap": false }
  }
}
```

A file's existing indentation wins over `tab_size`/`hard_tabs`. A project's `.editorconfig`
wins over both: `indent_style`, `indent_size`, `tab_width`, `trim_trailing_whitespace` and
`insert_final_newline` are supported, read up to the nearest `root = true`.

## Project settings

A project can carry its own settings in `.kuana/settings.json`, committed with the code, so
everyone who opens it gets the same indentation, formatters, language server settings and run
configurations (like Zed's `.zed/settings.json`). **Open Project Settings** (Find Action) creates
it from a commented template. For files in that folder its values go over your own
`settings.json`; a `.kuana/settings.json` in a subfolder (a package in a monorepo) goes over the
project's for files under it. Saving the file applies it, and so does a change from outside Kuana
(a checkout, a pull).

It takes the editor options (`tab_size`, `hard_tabs`, `soft_wrap`, `format_on_save`, `rulers`,
`trim_trailing_whitespace`, `insert_final_newline`, …), `languages`, `language_servers`, `lsp`,
`formatters` and `run_configurations`, written as in `settings.json`. Everything else (theme,
fonts, keymap, Vim mode, auto-save…) stays yours; such keys are reported and ignored.

- Editor options resolve per file: built-in defaults, then your top-level options, then your
  `languages` entry, then the project's top-level options, then its `languages` entry, then
  `.editorconfig`. So a project's `"tab_size": 2` applies even where you set a tab size for
  that language yourself.
- `language_servers` and `formatters` entries replace yours for the same language; `lsp`
  settings and initialization options merge key by key (a project's `binary` replaces yours);
  the project's `run_configurations` are listed before yours, their `cwd` relative to the project.
- Until you [trust](#project-trust) the project, only its editor options and `languages` apply:
  the rest names programs to run.

```jsonc
// .kuana/settings.json
{
  "tab_size": 2,
  "format_on_save": true,
  "languages": { "Python": { "tab_size": 4, "rulers": [88] } },
  "formatters": { "Python": ["ruff", "format", "-"] },
  "lsp": { "rust-analyzer": { "initialization_options": { "check": { "command": "clippy" } } } },
  "run_configurations": [{ "name": "Dev server", "command": "npm run dev" }]
}
```

With the JSON language server installed, the file gets completion and checks from a schema of
just these keys.

## Project trust

A cloned repository can make an editor run its code without asking: a language server builds it,
ESLint and Prettier load its JavaScript config, `node_modules/.bin` and venv tools are its own
programs, and Git runs commands its config names (an fsmonitor, hooks, an SSH command, diff programs). So the first time you open a
folder, Kuana asks whether you trust it. Until you do, it stays in **Restricted Mode** (shown in the
title bar): language servers don't start, formatters and ESLint don't run, the folder's own tools
aren't used, terminals don't activate its venv, and Git leaves out what the folder's own config would run (its fsmonitor, hooks, SSH command, credential helper and external diff). Editing, Git, Run
and Debug work as usual.

**Trust Folder** remembers the folder; tick **Trust every project in …** to trust its parent
folder instead (for example the folder you keep your repositories in). Click **Restricted Mode**
in the title bar or use **Project Trust…** to decide later or to stop trusting a folder.
**Settings → Trust** lists trusted folders, and `trust_all_projects` turns the checks off.
