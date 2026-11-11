# Extensions

Extensions add languages, themes, icon themes and snippets without rebuilding Kuana. They use
[Zed's extension format](https://zed.dev/docs/extensions/developing-extensions), so Zed's language
and theme extensions work as they are, and VS Code snippet files too.

**Install** from **Settings → Extensions** (or **Extensions…** in the Kuana menu, Find Action and the puzzle at the top right):
paste the URL of an extension's Git repository and press **Install**, or **Install from Folder…**
for one on disk, which is used in place (for making your own: edit it, then **Reload**). Each
extension is listed with what it adds and any problems; the trash button removes it. If Zed is
installed, **Installed in Zed** lists its extensions with an **Add** button: those come with their
grammars prebuilt, so nothing needs compiling. Extensions live
in `~/.config/kuana/extensions/<id>/`; copying a folder there and choosing **Reload Extensions** works
too.

**Available** lists the extensions in Kuana's registry
([kuana-extensions](https://github.com/berkayakin/kuana-extensions)): popular languages that Kuana
doesn't have built in, such as Java, C#, Dart, Ruby, Elixir, Zig and Vue, and Oxie and Lyth. Each is built there from its
author's repository, with its grammars and its own code compiled, so nothing needs compiling here. Type
to search, then click **Install**. **Update** appears when the registry has a newer version; only
extensions installed from it get one, never one from Git or a folder. Kuana checks the list's
signature and each download's checksum. Every extension keeps its own license (shown on its row),
which comes with it.

An extension is a folder like this (Zed's layout; every part is optional):

```text
my-extension/
  extension.toml
  languages/mylang/config.toml
  languages/mylang/highlights.scm     tree-sitter highlight query
  languages/mylang/outline.scm        File Structure, breadcrumbs, sticky scroll
  languages/mylang/injections.scm     embedded languages (CSS in <style>, SQL in strings…)
  languages/mylang/brackets.scm       bracket pairs, words too (do … end)
  languages/mylang/indents.scm        what Enter indents after; @end / @outdent go back out
  languages/mylang/textobjects.scm    functions and classes for Vim's af / if / ac / ic
  languages/mylang/runnables.scm      gutter ▶ for tests and mains, with tasks.json
  languages/mylang/tasks.json         commands for runnables' tags ($ZED_FILE, $ZED_ROW…)
  grammars/mylang.wasm                a prebuilt grammar (Zed's packaged extensions have one)
  themes/*.json                       Kuana themes, or Zed theme families
  icon_themes/*.json                  file icon themes (Zed's format)
  snippets/<language id>.json         VS Code / Zed snippets
```

```toml
# extension.toml
id = "mylang"
name = "MyLang"
version = "0.1.0"
description = "MyLang support"
authors = ["You <you@example.com>"]

# The grammar: fetched from a repository at a commit, or
# without `repository`, the sources in grammars/mylang/ (src/parser.c…).
[grammars.mylang]
repository = "https://github.com/someone/tree-sitter-mylang"
rev = "0123456789abcdef0123456789abcdef01234567"
# path = "dialect"   # the grammar's folder inside the repository

# A language server for this extension's languages, or for a built-in one
# (it then runs beside Kuana's). Zed extensions find their server in code,
# which Kuana runs for extension API 0.6 and 0.7 (see below). Otherwise, or
# when that code fails, Kuana runs `command`, else a well-known server's
# command (zls, gleam lsp, nil, lua-language-server, haskell-language-server…),
# else the server's id as a program. With several servers for a language in
# one extension, the first one installed (or that the code starts) runs.
# Settings' "language_servers" and "lsp" can always override.
[language_servers.mylang-ls]
name = "MyLang LS"
languages = ["MyLang"]
command = "mylang-ls"
args = ["--stdio"]
install = "brew install mylang-ls"   # shown when it isn't found
# A debug adapter, for configurations naming its id or an alias in launch.json
# or .zed/debug.json (Kuana's keys: Zed extensions start theirs in code). With
# `tcp`, it listens on the port Kuana puts in its arguments as {port}.
# [debug_adapters.mylang-dap]
# command = "mylang-dap"
# args = ["--listen", "{port}"]
# tcp = true
# aliases = ["MyLang"]

# What Zed extensions' code would pass the server (Kuana's keys):
initialization_options = { lint = true }
settings = { mylang = { formatting = "auto" } }

# How the status bar's Install gets it (Kuana's key; checksummed like Kuana's own):
# an npm package, a Go module, or a GitHub release asset per platform (the binary, gzipped,
# or in a .zip or tarball such as cargo-dist's .tar.xz).
[language_servers.mylang-ls.installer]
github = "someone/mylang-ls"
assets = { "macos-aarch64" = "mylang-ls-darwin-arm64.gz", "linux-x86_64" = "mylang-ls-linux-x64.gz" }
# npm = "mylang-language-server"
# go = "example.com/mylang/cmd/mylang-ls"
```

```toml
# languages/mylang/config.toml
name = "MyLang"                 # as the status bar and settings' "languages" call it
grammar = "mylang"
path_suffixes = ["ml", "MyLangfile"]   # extensions or whole file names
line_comments = ["// "]
block_comment = ["/* ", " */"]
tab_size = 2
hard_tabs = false
formatter = ["mylang-fmt", "-"] # Kuana only: reads stdin, writes stdout ({file} is the path)
# Pairs typing closes (one character each); not in strings or comments when not_in says so
brackets = [
    { start = "{", end = "}", close = true, not_in = ["string", "comment"] },
    { start = "\"", end = "\"", close = true, not_in = ["string"] },
]
# Lines that go back a level as they're typed, level with the nearest line above starting with a word of valid_after
decrease_indent_patterns = [{ pattern = "^\\s*(else|end)\\b", valid_after = ["if", "do"] }]
```

Without `brackets`, typing pairs `()`, `[]`, `{}` and quotes outside strings and comments. Editing follows the
language at the caret: in an embedded language (a `<script>`, a Markdown fence) its comments, pairs and
indentation apply.

Without `highlights.scm` (or `outline.scm`), the grammar's own `queries/highlights.scm` (and
`tags.scm`) are used. A language from an extension gets highlighting, File Structure, breadcrumbs,
sticky scroll, comment toggling, its indentation, snippets, its language server and formatter,
and a row in **Settings → Languages**. The grammar's parse errors show in Problems only once
turned on there (`syntax_errors`), since a grammar may reject valid code; Zed doesn't show them
either. Built-in languages win for their file extensions, except that a language naming an exact
file (`docker-compose.yml`) gets it.

A language named like a built-in one (`name = "Rust"`) without a grammar of its own changes the
built-in language's queries instead: each of its `.scm` files replaces Kuana's of the same kind, and the
others stay. Kuana's own are in [`assets/languages`](../assets/languages), in this same format.

Tested with Zed's Gleam, Elixir, Zig, Dockerfile, Lua, Nix, Kotlin and Haskell extensions and the
Catppuccin themes, installed from their repositories.

To check an extension while making it, run `kuana extension check <folder>` (or the binary inside
Kuana.app). It reads `extension.toml` and each `config.toml` (reporting keys Kuana doesn't know, with a
suggestion for typos), builds the grammars as Kuana would, and compiles every query against its
grammar: errors come with their file and line, and captures without a color in Kuana's themes,
or queries missing the captures Kuana uses (`@open`/`@close`, `@indent`, `@run`…), are warnings. It
exits with 1 when there are errors. `schema_version = 1` in `extension.toml` (Zed's) is the format
this Kuana reads.

Grammars run as WebAssembly, sandboxed in Kuana's runtime. One that comes prebuilt
(`grammars/<name>.wasm`, which the extensions Zed installs include) runs as is. Otherwise it's compiled
the first time, in the background, with [wasi-sdk](https://github.com/WebAssembly/wasi-sdk): Kuana
asks before downloading it once (about 180 MB, 170 MB on disk in `~/.config/kuana/toolchains/`;
`WASI_SDK_PATH` uses one you have). Files of that language are highlighted when it's done.
Fetching uses `git`. With `native_grammars` on, grammars of extensions installed from a folder
(Install from Folder…, on macOS and Linux) that's in a trusted folder are compiled with your C
compiler instead (`cc`; on macOS `xcode-select --install`), which C++ scanners need. Extensions
installed from the registry or Git always run their grammars as WebAssembly.

Zed extensions' own WebAssembly code (`extension.wasm`) runs for their language servers when it's
built for extension API 0.6 or 0.7 (`[lib] version` in `extension.toml`): Kuana asks it for the
command, which it may download first (into `~/.config/kuana/extensions/work/<id>/`, the only folder
it can reach, removed with the extension), the initialization options and the workspace
configuration. `lsp.<id>` settings go by Zed's server id; as in Zed, a server's `settings` reach it
through the extension's code (which may put them in a section of their own), asked again when they
change. It runs sandboxed in the same runtime as grammars, with limits on how long it computes and
how much memory it takes; it can reach the network only through Kuana (GitHub releases, HTTP, npm
with Kuana's Node.js), and runs programs only for trusted projects. A built-in language's own server
still comes first, so an extension's server for the same program (PHP's intelephense, HTML's)
doesn't start beside it. Extensions' other code (slash commands, context servers, debug adapters,
completion labels) isn't run.

Icon themes are chosen with `icon_theme` (Settings → Appearance → Icon theme); without one, Kuana uses
its built-in Charmed Icons.

## Views, commands and panels

Beyond Zed's format, an extension can add to Kuana's UI: **views** on the activity bar (a tree of
items, like Files or Database), **commands** (in Find Action, view toolbars, tree items' right-click
menus and the editor's right-click menu), and **panels** in the bottom area that show text or a table.
Kuana draws all of it in its own style; the extension's code only supplies the items and runs the
commands. A complete example is [`examples/extensions/docker`](../examples/extensions/docker): a
Docker containers view with Start, Stop, Show Logs and Inspect.

The extension declares what it adds in a `[kuana]` table of its `extension.toml`. Zed ignores it, so
the extension still works in Zed for everything else:

```toml
[kuana]
api = "0.1"                       # the interface version its code is built for
permissions = ["process"]         # network, process, workspace-files

[kuana.views.containers]
title = "Containers"
icon = "icons/container.svg"      # an SVG in the extension, or one of Kuana's icon names
when = "always"                   # or "file:compose.yaml" (in the project), "language:Go" (an open file)

[kuana.commands.refresh]
title = "Refresh"
icon = "refresh-cw"

[kuana.commands.logs]
title = "Show Logs"

[[kuana.menus]]
place = "view-toolbar:containers" # view-toolbar:<view>, view-item:<view> or editor
command = "refresh"

[[kuana.menus]]
place = "view-item:containers"
command = "logs"
when = "kind == 'running' || kind == 'stopped'"   # editor menus: language == 'YAML'

[kuana.panels.logs]
title = "Container Logs"
```

Its code is `kuana.wasm` beside `extension.toml`: a WebAssembly component (a Rust `cdylib` built
with `cargo build --release --target wasm32-wasip2`) for the `extension` world in
[`kuana.wit`](../src/extensions/kuana-wit/kuana.wit). Copy that file into your extension and
generate bindings with [`wit-bindgen`](https://crates.io/crates/wit-bindgen). The code exports:

- `children(view, parent)`: a view's items, the roots or an item's children (asked when it opens).
  Each item has a stable id, a label, an optional detail, icon, tone (`success` colors it green,
  `error` red, `muted` dims it), a `kind` that menus match, whether it has children, and a command
  run when it's clicked.
- `run-command(command, target)`: runs a command, on a tree item's id, or on the current file's path
  for an editor menu. An error it returns is shown to the user.
- `activate()`, before anything else, and `file-saved(path)`, after the user saves a file in the
  extension's work folder.

And calls Kuana back: `refresh` a view, `show-panel` (lines of text or a table), `open-file`,
`notify`, `ask-input` (a text or password prompt), `confirm`, `settings` (its `extensions.<id>` entry
in settings.json), `project-root`, `secret-get` / `secret-set` (the system keychain, its own keys
only) and `set-diagnostics` (problems shown in a file, and in Problems).

The code runs in the same sandbox as Zed extensions' code: one folder of its own
(`~/.config/kuana/extensions/work/<id>/`, its current directory), and limits on computing time,
memory and how long a call takes. Anything more needs a permission it declares, and Kuana asks the
user the first time the code uses one:

| Permission | Lets the code | Calls |
| --- | --- | --- |
| `process` | run programs, in trusted projects only | `run-process`, `run-in-terminal` |
| `network` | make HTTP requests | `http-request` |
| `workspace-files` | read the open project's files | `read-project-file`, project paths in `open-file` |

Answers are remembered; Settings → Extensions shows an extension's permissions with a **Revoke**
link. Run `kuana extension check <folder>` to check the `[kuana]` table and load the code.

Commands can be bound to keys in `keymap.json`, by the extension's id and the command's:

```json
{ "bindings": { "cmd-shift-d": ["workspace::RunExtensionCommand", { "extension": "docker-view", "command": "refresh" }] } }
```

`workspace::ToggleExtensionView` (`{ "view": "docker-view/containers" }`) shows or hides a view.

Kuana reads Zed's extension format for compatibility; it isn't affiliated with or endorsed by Zed
Industries. Extensions come from their authors' repositories, under their own licenses.
