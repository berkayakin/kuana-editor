# Languages, formatting and environments

## Language servers

Kuana starts a language server the first time you open a file it handles, one per server and
project, and shows it in the status bar (with indexing progress). It uses the server in the
project's `node_modules/.bin` or venv, else one on your PATH (also the login shell's PATH,
`~/.cargo/bin`, `~/go/bin`, Homebrew and nvm, so it works when Kuana is opened from the Dock).

A language Kuana doesn't have built in comes with an [extension](extensions.md): install one from
**Settings → Extensions → Available**. Most bring their own code for their server, which uses the
server you have installed or downloads one itself (into `~/.config/kuana/extensions/work/<id>/`).

If none is installed, the status bar offers **Install <server>**: one click puts it in Kuana's own
folder (`~/.config/kuana/servers/`) and starts it. While Kuana downloads anything (a server, Node.js,
wasi-sdk, an update), the status bar shows its progress; the tooltip lists each download's size,
progress and speed. rust-analyzer, taplo, marksman and phpactor come from their
GitHub releases (checked against GitHub's SHA-256); vtsls, basedpyright, intelephense, the
vscode-langservers (HTML, CSS, JSON), yaml-language-server and bash-language-server are npm
packages, installed with your Node.js or, without one, a Node.js LTS Kuana downloads once (checked
against its SHASUMS256.txt); gopls comes from `go install`. A server you install yourself is
always used instead, and Kuana looks for a newer version of its own copies once a week (it runs from
the next start). An extension can bring an installer for its server too: Oxie's downloads the `oxie`
command from its GitHub release. Servers without an installer show **No language server**; click it
to copy the install command.

| Language | Server | Install |
| --- | --- | --- |
| Rust | rust-analyzer | `rustup component add rust-analyzer` |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` |
| JavaScript, TypeScript | typescript-language-server or vtsls | `npm install -g typescript typescript-language-server` |
| Python | basedpyright, pyright or pylsp, with Ruff beside it | `pip install basedpyright ruff` |
| PHP | phpactor (needs PHP) or intelephense | `npm install -g intelephense` |
| HTML, CSS, JSON | vscode-langservers-extracted | `npm install -g vscode-langservers-extracted` |
| YAML | yaml-language-server | `npm install -g yaml-language-server` |
| TOML | taplo | `cargo install taplo-cli --locked --features lsp` |
| Shell | bash-language-server | `npm install -g bash-language-server` |
| Markdown | marksman | `brew install marksman` |

Servers report problems either by sending them as they find them or by being asked (pull
diagnostics, `textDocument/diagnostic`: basedpyright, pyright and ESLint work this way when the
editor supports it); Kuana does both. It asks when a file opens and a moment after each edit, and,
for a server whose problems span files, asks again for its other open files too.

basedpyright checks in pyright's `"standard"` mode, as in Zed (its own default, `"recommended"`,
warns about most untyped code). A project's `pyrightconfig.json` or `[tool.basedpyright]` in
`pyproject.toml` sets its own; to change it everywhere, set
`"lsp": { "basedpyright": { "settings": { "basedpyright.analysis": { "typeCheckingMode": "recommended" } } } }`.

As in Zed, Python files also get Ruff's language server (`ruff server`) when Ruff is in the project's
venv or on PATH: its lint rules (with your `[tool.ruff]` settings) and their fixes, and Organize
Imports, which basedpyright then leaves to it. Without Ruff installed nothing is shown for it. PHP
files get phpactor, as in Zed, when PHP is installed to run it (on Windows, and without PHP,
intelephense, which reports many more missing types and methods).

Renaming or moving a file or folder in the file tree first asks the servers that want to know
(`workspace/willRenameFiles`) what else changes with it, and makes those edits before the rename:
rust-analyzer updates `mod` declarations and paths, TypeScript servers update imports. Edited files
are saved by auto-save (or listed to save, like a rename's). A server that takes longer than five
seconds is left out and a notification says so; servers hear of every rename, undo included.

With a server running, its diagnostics replace Kuana's built-in syntax check, and for JavaScript and
TypeScript the built-in rules too, since the server reports the same mistakes (the HTML and Markdown
rules and ESLint still run). Hover a symbol for its documentation and any problem there (the
popover scrolls, and a copy button that appears on hover copies its text); completion opens as you type (Enter inserts, Tab replaces the rest of the word). **Restart
Language Server** (Find Action) restarts it after installing or when it's stuck. To use another
server or turn one off, see `language_servers` in [Settings](settings.md); to configure one, `lsp` (Zed's format).

A problem shows at the end of its line, cut short when it's long. With the caret on that line, a
problem that doesn't fit there (or several, or a multi-line message) shows in full in rows under it.
**Next / Previous Problem** (F2 / ⇧F2 in the JetBrains keymap, F8 / ⇧F8 in VS Code's and Zed's)
moves the caret through the file's problems, the language servers' and the built-in checks' alike.
As in Zed, only errors and warnings are problems: a server's information is underlined and its hints
faintly, and both show on hover, but they aren't counted, listed in Problems or stepped through.
Code a server marks as unused (an unused import or variable) is faded, and underlined only when it's
reported as a warning or error.

A file can have several servers: Kuana's own and any an extension adds for its language (ESLint or
Tailwind beside TypeScript). All of them get the file; their problems show together, completion
and code actions list all of theirs, and the rest (hover, Go to Declaration, rename, formatting)
goes to the first that offers it. The status bar names the ones running.

**Find Usages** (⌥F7 / Alt+F7 in the JetBrains keymap, ⇧F12 in VS Code's) lists a symbol's usages
in the **Find** panel at the bottom, grouped by file, each with its line and the usage highlighted.
↑/↓ show each one in the editor while the panel keeps the keyboard, Enter (or a click) goes there,
←/→ collapse and expand a file, and Escape goes back to the editor. **Next / Previous Result**
(F4 / ⇧F4 in the VS Code keymap, or Find Action) steps through them from anywhere. A new search
replaces the panel's tab unless you pin it (the pin at its top right); pinned searches stay as
tabs. **Toggle Find Panel** (⌘3 / Alt+3 in the JetBrains keymap, View → Find Panel) shows it again,
and the title bar has a toggle once it has results. Go to Declaration on a declaration still lists
its usages in a quick popup, as in JetBrains.

**Call Hierarchy** (⌃⌥H / Ctrl+Alt+H in the JetBrains keymap, ⇧⌥H in VS Code's, or the editor's
right-click menu) shows the function at the caret in the Find panel with its **Callers** (open) and
the functions it **Calls**; each of those opens onto its own callers or calls, asked for only when
you open it, so you can follow a chain as deep as you like. A caller opens at its call, with the
number of calls when there are several. **Type Hierarchy** (⌃H / Ctrl+H in the JetBrains keymap)
does the same for a type's **Supertypes** and **Subtypes**. Both need a server that offers them
(rust-analyzer and pyright have call hierarchy; gopls and clangd both); otherwise a message says so.

**Code lenses** sit above lines, as the server offers them: rust-analyzer's ▶ Run / Debug (run in a
Run tab or the debugger, like the gutter ▶), implementations and references (which open the
usages list), and other servers' commands. A lens's title, which can cost the server a search, is
asked for as it comes into view; its row is there meanwhile. Turn them off with `code_lens`.

**Inlay hints** show what the server infers inside the code, on a soft chip: variable types,
parameter names at call sites and the like. Kuana turns them on in typescript-language-server, vtsls
and gopls, which leave them off unless asked (change that through `lsp`). They're asked for the
lines in view and around them, so a long file costs no more than a short one. Turn them off with
`inlay_hints` or, for one editor, **Toggle Inlay Hints**.

## Formatting

**Reformat Code** formats the file, or the selection when the language server supports range
formatting. Kuana picks, in order: a formatter installed in the project (`node_modules/.bin`, e.g.
Prettier), the language server, then a formatter on PATH: Prettier (JS, TS, CSS, HTML, JSON,
YAML, Markdown), rustfmt (with the edition from Cargo.toml), gofmt, ruff or black, taplo, shfmt.
Only the changed lines are replaced, so the caret stays put and one undo reverts it. Formatters
read their own config (`.prettierrc`, `rustfmt.toml`, `pyproject.toml`…). Set `format_on_save`
(per language too) to format on Save; choose a formatter per language with `formatters`.

## Python environments

Kuana finds the project's virtual environment: a `.venv`, `venv`, `env` or `.env` folder holding
`pyvenv.cfg`, next to the file or in a folder above it. New terminals activate it (its activate
script, typed in like you would), Run tabs and the debugger use its Python, and language servers
and formatters start with it first on PATH, so they see its packages and are found when installed
in it. The status bar shows it for Python files; click it to pick another venv in the project, a
folder elsewhere (**Choose Folder…**) or **None**. The choice is remembered per project.
