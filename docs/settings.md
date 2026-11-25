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
| `editor_font_family` | `"JetBrains Mono"` (bundled) | any installed font; the terminal uses it too unless `terminal_font_family` is set |
| `ui_font_size` | `13` | the window's own text: tabs, menus, panels (10 to 18); the file tree's names are a point larger; Settings → Appearance |
| `editor_font_size` | `14` | the zoom actions adjust it on top (remembered between launches) |
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
| `linked_editing` | `true` | typing in a tag's name (HTML, JSX, and grammars like them) renames its closing or opening tag too |
| `cursor_blink` | `true` | the caret blinks; 10 seconds after it last moved it stops, staying on, so an idle window doesn't redraw twice a second; `false` keeps it on |
| `semantic_tokens` | `true` | the language server's semantic tokens over the syntax colors (types, parameters, macros; mutable variables underlined), for servers that send them |
| `document_colors` | `true` | a swatch before each color in CSS and HTML (and colors language servers find); click it for a picker |
| `inline_values` | `true` | paused in the debugger, variables' values after the lines of the function that name them |
| `code_lens` | `true` | the language server's code lenses above lines ("3 references", "▶ Run \| Debug") |
| `minimap` | `false` | the document in miniature at the editor's right edge, in place of the scrollbar: code as colored blocks, the visible part as a slider, problems, search matches and Git changes along its edge |
| `minimap_width` | `100` | the minimap's width in pixels (40–240) |
| `syntax_errors` | `true` (`false` for extension languages) | parse errors in Problems when no language server covers the file. Extensions' grammars may not know all valid code (tree-sitter-sql rejects Postgres' `ON CONFLICT (…)`), so turn it on per language for the ones you trust, e.g. `"languages": {"Gleam": {"syntax_errors": true}}` |
| `edit_predictions` | `true` | ghost-text suggestions from `edit_prediction_provider`; `false` turns them off per language or in a project's settings |
| `languages` | | per-language overrides of the thirteen above (all but `minimap_width`), by the name in the status bar |
| `vim_mode` | `false` | Vim's modes and keys; see [Vim mode](editing.md#vim-mode) |
| `todo_patterns` | `["TODO", "FIXME", "HACK", "XXX"]` | words that make a comment a TODO item, in any case; see [TODO list](editing.md#todo-list) |
| `edit_prediction_provider` | `"none"` | where ghost-text suggestions come from: `"none"`, `"copilot"` (GitHub Copilot) or `"ai"` (your [AI model](ai.md#tab-completion)); Settings → Editor → Edit predictions. Only your own settings can choose one, not a project's; see [Edit predictions](editing.md#edit-predictions) |
| `claude_code_integration` | `true` | Claude Code running in a terminal (its `/ide` command) sees the selection in Kuana, opens files here, reads the problems Kuana shows and proposes edits as diffs; local projects only. Settings → Editor → Claude Code; see [Claude Code](editing.md#claude-code) |
| `gemini_cli_integration` | `true` | Gemini CLI running in a terminal (after its `/ide enable`) knows which files are open in Kuana and what's selected, and proposes edits as diffs; local projects only. Settings → Editor → Gemini CLI; see [Gemini CLI](editing.md#gemini-cli) |
| `agent_notifications` | `true` | a system notification when an agent session finishes or needs your answer while its window isn't in front (macOS and Linux; on Windows the taskbar button flashes); clicking it brings the session back. Settings → AI → Sessions; see [Agent sessions](ai.md#agent-sessions) |
| `agent_keep_awake` | `true` | the computer doesn't idle-sleep while an agent session works (the display still may). Settings → AI → Sessions |
| `ai` | none | The model provider for Kuana Agent, Inline Assist and generated commit messages: `provider` (`anthropic`, `openai`, `openrouter`, `gemini`, `mistral`, `deepseek`, `xai`, `groq`, `ollama`, `lmstudio`, `custom`), `model`, `effort` (`low` … `max`, where the model takes it) and `providers.<id>.url` for another endpoint. `completion_model` is a faster model for tab completion (else `model`); `mcp_servers` names MCP servers agent sessions get (`command`, `args`, `env`). Keys are kept in the system keychain (Settings → AI) or read from the provider's environment variable. Only your own settings can set it; see [AI](ai.md) |
| `icon_theme` | built-in | an extension's icon theme by name, e.g. `"Soft Charmed Icons"` |
| `visual_effects` | `"full"` | how much decoration the interface draws: `"full"`, `"reduced"` (solid colors instead of gradients) or `"minimal"` (also no shadows, glows or animations); Settings → Appearance. The older `"reduce_visual_effects": true` still works, as `"minimal"` |
| `ui_style` | `"modern"` | the window's layout: `"modern"` (rounded islands with gaps between them, browser-like tabs) or `"classic"` (square areas side by side, separated by lines, rectangular tabs); Settings → Appearance → Layout, Welcome Setup, or Toggle Classic Layout in Find Action |
| `sidebar_dock` | `"left"` | which side of the editor the sidebar (Files, Commit) sits on: `"left"` or `"right"`; Settings → Appearance, Move Sidebar to Right / Left on its tabs' right-click menu, or Toggle Sidebar Position. A single view moves by itself: right-click its tab → Move to Right / Left Side puts it in a second sidebar on the editor's other side (its header has the view's name, Move Back and Close; with several views there, the name opens their list; Toggle Second Sidebar shows or hides it) |
| `open_folder_in` | `"ask"` | what Open Folder… does while a project is open: `"ask"` (a new window, this window, or add it to the project), `"new_window"`, `"this_window"` or `"add_to_project"`; Settings → Editor → Projects. See [Several folders in one window](editing.md#several-folders-in-one-window) |
| `project_avatar` | `true` | the round initial before the project's name in the title bar; Settings → Appearance → Layout |
| `compact_activity_bar` | `false` | a narrower activity bar beside the sidebar, with smaller buttons and icons (the title bar's buttons' size), leaving more room for the sidebar and editor; Settings → Appearance → Layout, or Toggle Compact Activity Bar in Find Action |
| `tools_location` | `"activity_bar"` | where the tool buttons (Run, Debug, the panels' toggles, Search Everywhere, Extensions, Settings) sit: `"activity_bar"` (a column beside the sidebar, its views as vertical tabs) or `"title_bar"` (a title bar row of their own, with the project and branch, above the tab row; no activity bar, the sidebar's views are tabs like the editor's); Settings → Appearance → Layout, Welcome Setup, or Toggle Tools in Title Bar in Find Action |
| `zen_full_screen` | `true` | [Zen mode](editing.md#zen-mode) makes the window full screen; Settings → Appearance → Zen mode |
| `zen_width` | `100` | Zen mode's centered text column, in characters (40–240) |
| `zen_line_numbers` | `false` | line numbers stay in Zen mode |
| `zen_tabs` | `false` | the editor's tabs stay in Zen mode |
| `language_servers` | | per language: `false` turns its server off, `["cmd", "args"]` replaces it |
| `extensions` | | per extension, by id: settings its code reads (see [Writing view extensions](extension-ui.md#settings)), as the extension documents them |
| `lsp` | | per server, by id (Kuana's, e.g. `typescript`, or Zed's, e.g. `vtsls`, `basedpyright`): `settings` (what it asks for through `workspace/configuration`; a change is sent to it), `initialization_options` (applied when it restarts) and `binary` (`{ "path", "arguments" }`) |
| `terminal_font_family` | the editor's font | any installed font for the terminal, e.g. a Nerd Font for your prompt's icons; Settings → Terminal |
| `terminal_font_size` | the editor's size − 1 | the terminal's font size; the zoom actions adjust it on top |
| `terminal_shell` | login shell | the terminal's shell and arguments, e.g. `["/opt/homebrew/bin/fish", "--login"]` |
| `terminal_option_as_meta` | `false` | macOS: Option acts as Meta in the terminal, sending Escape and the key (Option+B is the shell's word back) instead of typing a character like `∫`; Settings → Terminal. Alt always does on Windows and Linux |
| `terminal_shell_integration` | `true` | zsh, bash and fish mark each command: how it ended at the terminal's edge, jumping between commands, copying one's output, links relative to the shell's folder; Settings → Terminal. New terminals only |
| `run_configurations` | | `[{ "name", "command", "cwd" }]` added to Run…; see [Run and test](run-and-debug.md#run-and-test) |
| `formatters` | | per language: `"language_server"`, a command reading stdin and writing stdout (`{file}` is the path), or `false` |
| `trust_all_projects` | `false` | run language servers, formatters and project tools in every folder without asking; see [Project trust](#project-trust) |
| `native_grammars` | `false` | compile extensions' grammars to native code with your C compiler instead of WebAssembly (only for extensions installed from a folder in a trusted folder) |
| `auto_update` | `true` | download new releases in the background; see [Releases and updates](install.md#updates) |
| `auto_update_extensions` | `{}` | per extension id, `false` keeps one installed from the registry at its version; the others update by themselves ([Extensions](extensions.md)) |
| `auto_fetch` | `true` | fetch from the remotes every few minutes while the window is in use (trusted projects only, failures stay quiet), so commits waiting to be pulled show early; see [Git](git.md) |
| `auto_resolve_conflicts` | `true` | when a pull, merge or rebase stops on conflicts, combine the simple ones (the two sides changed different words, or made the same change) and mark files with none left resolved; see [Git](git.md) |
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

## Importing from VS Code or JetBrains

**Import Settings…** (File menu, Find Action, or **Import from …** in Welcome Setup) brings your
setup from another editor on this computer: Visual Studio Code (and Insiders, VSCodium, Cursor,
Windsurf) or a JetBrains IDE (its newest version). Kuana lists what it found, then shows exactly
what it would change before writing anything.

- **From VS Code:** the VS Code base keymap; font, font size and line height; tab size, tabs or
  spaces, soft wrap, rulers, whitespace marks; format on save, trimming and the final newline;
  auto-save; the minimap, inlay hints, code lenses, linked editing, color swatches and semantic
  highlighting; the theme when Kuana has one of that name; the sidebar's side; per-language
  settings (`"[python]": {…}`) for Kuana's built-in languages; your own `keybindings.json`
  shortcuts for commands Kuana has, in the editor or terminal when theirs were; your snippets
  (the same format, copied as they are; a file Kuana's snippets already have isn't replaced);
  Vim mode if you use VSCodeVim. Extensions that bring a language Kuana doesn't have built in
  (Java, C#, Dart, Ruby, Zig, Vue and more) are installed from Kuana's registry.
- **From a JetBrains IDE:** the editor font and size, a light theme if yours is light, soft wrap,
  its keymap if you switched it to VS Code's or Sublime Text's (JetBrains' is Kuana's own), and
  Vim mode if you use IdeaVim.

Settings that have nothing like them in Kuana are left out, and the preview says how many
shortcuts couldn't be taken. Settings go into `settings.json` and shortcuts into `keymap.json`, so
you can change or remove anything afterwards. A project's `.vscode/launch.json` already works in
Kuana as it is, under Debug….

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
aren't used, Copilot gets none of its files, terminals don't activate its venv, and Git leaves out what the folder's own config would run (its fsmonitor, hooks, SSH command, credential helper and external diff). Editing, Git, Run
and Debug work as usual.

**Trust Folder** remembers the folder; tick **Trust every project in …** to trust its parent
folder instead (for example the folder you keep your repositories in). Click **Restricted Mode**
in the title bar or use **Project Trust…** to decide later or to stop trusting a folder.
**Settings → Trust** lists trusted folders, and `trust_all_projects` turns the checks off.
