# Writing view extensions

An extension can add to Kuana's UI:

- **views** on the activity bar: a tree of items, like Files or Database;
- **commands**, in Find Action, a view's toolbar, the right-click menus of its items, the editor, tabs and the file
  tree, with a default shortcut if you give one;
- **panels** in the bottom area, showing lines of text or a table;
- **status bar items**, and its own **settings**.

Its code can also read and edit the open file, write project files, pick from a list, and hear about files being
opened and saved or a timer coming due. Those need interface 0.2: see [Interface 0.2](#interface-02).

Kuana draws all of it in its own style and handles the keyboard, scrolling and themes. Your code answers with data
("these are the items", "this command did that") and asks Kuana for things through host calls (open a file, show
a panel, ask the user). It's written in Rust and compiled to WebAssembly, and runs in a sandbox.

This page walks through a small extension from an empty folder, then lists everything in [the reference](#reference).
For a larger example, see [`examples/extensions/docker`](../examples/extensions/docker): a Docker containers view with
logs (shown once, or followed as they come), inspect, start and stop.

## Tutorial: a Notes view

The extension adds a Notes view. It lists notes, opens one when clicked, creates one from the toolbar, deletes one
from its right-click menu, and shows a table of word counts in a panel. It needs no permissions: notes live in the
extension's own folder, the one place its code can write.

### 1. What you need

- Rust, with the WebAssembly target: `rustup target add wasm32-wasip2`
- Kuana, with its `kuana` command (Kuana menu → Install Shell Command) to check the extension

### 2. The folder

```text
notes/
  extension.toml     what the extension adds
  Cargo.toml         its code's crate
  wit/kuana.wit      Kuana's interface, copied from the Kuana repository
  src/lib.rs         the code
  kuana.wasm         the built code (step 6)
```

Copy [`src/extensions/kuana-wit/kuana.wit`](../src/extensions/kuana-wit/kuana.wit) from the Kuana repository into
`wit/`. It's licensed MIT OR Apache-2.0, so you can ship it with your extension.

### 3. `extension.toml`

The manifest says what the extension adds, so Kuana can show the view and its menus before any code runs. Kuana's
part is the `[kuana]` table; Zed ignores it, so the same extension can also carry languages or themes for both editors.

```toml
id = "notes"
name = "Notes"
version = "0.1.0"
schema_version = 1
description = "Quick notes in a view of their own."
authors = ["You"]

[kuana]
api = "0.1"

[kuana.views.notes]
title = "Notes"
icon = "file-text"

[kuana.commands.new]
title = "New Note"
icon = "plus"

[kuana.commands.open]
title = "Open Note"

[kuana.commands.delete]
title = "Delete Note"

[kuana.commands.stats]
title = "Show Word Counts"
icon = "table"

# Buttons in the view's header.
[[kuana.menus]]
place = "view-toolbar:notes"
command = "new"

[[kuana.menus]]
place = "view-toolbar:notes"
command = "stats"

# A note's right-click menu (items the code marks with kind = "note").
[[kuana.menus]]
place = "view-item:notes"
command = "delete"
when = "kind == 'note'"

[kuana.panels.stats]
title = "Notes"
```

### 4. `Cargo.toml`

The code is a `cdylib` crate. [`wit-bindgen`](https://crates.io/crates/wit-bindgen) turns `kuana.wit` into Rust: a
`Guest` trait to implement, and the host calls as functions.

```toml
[package]
name = "notes"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.63"

[profile.release]
opt-level = "s"
lto = true
strip = true
```

### 5. The code

`src/lib.rs`:

```rust
wit_bindgen::generate!({ path: "wit", world: "extension" });

use kuana::extension::{host, types::{PanelContent, Table, Tone}};

struct Notes;

/// The notes: `.md` files in the extension's own folder (its current directory).
fn notes() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md"))
        .collect();
    names.sort();
    names
}

impl Guest for Notes {
    fn activate() -> Result<(), String> {
        Ok(())
    }

    fn children(_view: String, parent: Option<String>) -> Result<Vec<TreeItem>, String> {
        // One level: no item has children.
        if parent.is_some() {
            return Ok(Vec::new());
        }
        Ok(notes()
            .into_iter()
            .map(|name| TreeItem {
                id: name.clone(),
                label: name.trim_end_matches(".md").to_string(),
                detail: None,
                icon: Some("file-text".into()),
                tone: Tone::Normal,
                kind: Some("note".into()),
                expandable: false,
                command: Some("open".into()),
            })
            .collect())
    }

    fn run_command(command: String, target: Option<String>) -> Result<(), String> {
        match command.as_str() {
            "new" => {
                let Some(name) = host::ask_input("Name of the new note", "", false) else { return Ok(()) };
                let file = format!("{}.md", name.trim());
                std::fs::write(&file, format!("# {}\n\n", name.trim())).map_err(|e| e.to_string())?;
                host::refresh("notes", None);
                host::open_file(&file, Some(3))?;
            }
            "open" => host::open_file(&target.ok_or("no note")?, None)?,
            "delete" => {
                let file = target.ok_or("no note")?;
                if host::confirm(&format!("Delete {file}?"), "It can't be brought back.", "Delete") {
                    std::fs::remove_file(&file).map_err(|e| e.to_string())?;
                    host::refresh("notes", None);
                }
            }
            "stats" => {
                let rows = notes()
                    .into_iter()
                    .map(|name| {
                        let words = std::fs::read_to_string(&name).unwrap_or_default().split_whitespace().count();
                        vec![Some(name), Some(words.to_string())]
                    })
                    .collect();
                let table = Table { columns: vec!["Note".into(), "Words".into()], rows };
                host::show_panel("stats", Some("Word counts"), &PanelContent::Table(table));
            }
            other => return Err(format!("unknown command {other}")),
        }
        Ok(())
    }

    fn file_saved(_path: String) -> Result<(), String> {
        host::refresh("notes", None);
        Ok(())
    }
}

export!(Notes);
```

What's going on:

- `children` gives the view's items. Kuana calls it for the roots (`parent` is `None`) and again for an item's
  children when it's opened, so a deep tree loads only as far as the user goes.
- An item's `id` must stay the same across calls: Kuana keeps which items are open and selected by it. Its `kind` is
  what menus match (`when = "kind == 'note'"`), and its `command` runs when it's clicked.
- `run_command` gets the command's id and its target: the item's `id` for item menus and clicks, the current file's
  path for editor menus, nothing for toolbar buttons and Find Action.
- The code's current directory is its own folder (`~/.config/kuana/extensions/work/notes/`), so plain `std::fs` calls
  work there. It can't reach anything else on its own.
- `refresh` tells Kuana to ask for items again. Kuana gathers refreshes that come close together into one.
- Returning an error from a command shows it to the user as a notification.

### 6. Build, check, install

```bash
cargo build --release --target wasm32-wasip2
```

```bash
cp target/wasm32-wasip2/release/notes.wasm kuana.wasm
```

```bash
kuana extension check .
```

The check reads the manifest, reports mistakes (an unknown command in a menu, a missing icon), and loads the code:

```text
note: [kuana]: 1 view, 4 commands, 1 panel
note: kuana.wasm: its code loads and activates
.: 0 errors, 0 warnings
```

Then **Settings → Extensions → Install from Folder…** and pick the folder. Kuana links to it rather than copying it,
so it uses your changes. A Notes icon appears on the activity bar.

### 7. Change it

- **Code:** build and copy `kuana.wasm` again. Kuana loads the new code at its next call; click the view's icon or
  run a command.
- **Manifest** (new commands, menus, icons): run **Reload Extensions** from Find Action.

### 8. When something goes wrong

- A command's error, or a view's, shows in Kuana: as a notification, or in place of the view's items.
- `println!` and `eprintln!` from the code go to Kuana's own output. Start Kuana from a terminal to see them.
- A call that runs too long or traps ends with an error; the next call starts the code afresh.
- `kuana extension check .` catches most mistakes in the manifest before you install.

## Reference

### The manifest

Everything is under `[kuana]` in `extension.toml`.

| Key | Meaning |
| --- | --- |
| `api` | The interface version the code is built for: `"0.1"` (the default) or `"0.2"`. See [Versions](#versions). |
| `permissions` | What the code may do beyond its own folder: `"process"`, `"network"`, `"workspace-files"`, `"workspace-write"` (0.2). See [Permissions](#permissions). |
| `events` | 0.2: what the code's `on-event` hears about. See [Events](#events). |
| `[kuana.views.<id>]` | A view. `title`; `icon`; `when`: `"always"` (the default), `"file:<name>"` (shown when the project has that file or folder, such as `compose.yaml`) or `"language:<name>"` (shown while a file in that language is open, such as `Go`). |
| `[kuana.commands.<id>]` | A command. `title`; `icon` (shown when it's a toolbar button); `key`: a default shortcut, see [Key bindings](#key-bindings). Find Action lists it as "<extension name>: <title>". |
| `[[kuana.menus]]` | Where a command shows. `place`: `"view-toolbar:<view>"`, `"view-item:<view>"`, `"editor"`, `"editor-tab"` (a tab's right-click menu) or `"file-tree"`. `command`: its id. `when`: see below. |
| `[kuana.panels.<id>]` | A bottom panel. `title`. |
| `[kuana.status.<id>]` | 0.2: a status bar item, filled by the code with `set-status`. `command`: run when it's clicked; `when`: as a view's. |
| `[kuana.settings.<key>]` | One of the extension's settings. See [Settings](#settings). |

A menu entry's `when` is one or more conditions joined with `\|\|`:

| Condition | Holds for |
| --- | --- |
| `kind == '…'` | a view item of that `kind` |
| `language == '…'` | a file in that language (editor and tab menus) |
| `name == '…'` | a file or folder with that whole name (`Makefile`) |
| `extension == '…'` | a file with that extension (`json`) |
| `type == 'folder'`, `type == 'file'` | a folder, or a file (the file tree's menu) |

In the editor's, a tab's and the file tree's menus the command's `target` is the file's path.

Ids are letters, digits, `-`, `_` and `.`.

**Icons** are either one of Kuana's own icon names, or a path to an SVG in the extension (`"icons/container.svg"`):

`archive`, `arrow-down`, `arrow-right`, `arrow-up`, `bolt`, `braces`, `bug`, `check`, `circle-alert`, `circle-check`,
`circle-x`, `clipboard`, `cloud`, `code-xml`, `copy`, `database`, `download`, `external-link`, `eye`, `file`,
`file-code`, `file-plus`, `file-text`, `folder`, `folder-open`, `function`, `git-branch`, `git-commit`, `hash`,
`history`, `key`, `link`, `minus`, `more`, `package`, `pencil`, `pin`, `play`, `plus`, `puzzle`, `refresh-cw`, `save`,
`search`, `settings`, `stop`, `table`, `tag`, `terminal`, `trash`, `triangle-alert`, `type`, `view-column`, `x` (and the
rest in [`assets/icons`](../assets/icons)).

An SVG of your own is drawn as a mask in the theme's colors, like Kuana's icons: one shape, any fill color. Google's
[Material Symbols](https://fonts.google.com/icons) (Rounded, filled) match Kuana's look.

### What the code exports

| Function | Called |
| --- | --- |
| `activate() -> result` | Once, when the code is loaded, before anything else. An error here stops the extension. |
| `children(view, parent) -> result<list<tree-item>>` | For a view's roots (`parent` is none) and when an item is opened. |
| `run-command(command, target) -> result` | When a command runs. `target`: the item's id (item menu, click), the file's path (editor menu), or none. |
| `file-saved(path) -> result` | 0.1 only: when the user saves (explicitly) a file in the extension's folder, for files the extension opened for editing. |
| `on-event(event) -> result` | 0.2, in place of `file-saved`: see [Events](#events). |

A **tree item**:

| Field | Meaning |
| --- | --- |
| `id` | Stable across calls; Kuana keeps open and selected items by it. Unique within its parent. |
| `label` | Its name. |
| `detail` | Dim text after the label (a type, a count, a state). |
| `icon` | One of Kuana's icon names, or an SVG path in the extension. |
| `tone` | `normal`, `muted` (dimmed), `success` (green), `warning`, `error` (red); colors its icon. |
| `kind` | What `view-item` menus match. |
| `expandable` | Whether it has children (Kuana shows a chevron and asks for them when it's opened). |
| `command` | A command run when it's clicked or Enter is pressed on it. Double-click opens or closes an item that has both. |

### Host calls

| Call | What it does | Needs |
| --- | --- | --- |
| `refresh(view, item)` | Asks for `item`'s children again, or the whole view when none. | |
| `show-panel(panel, title, content)` | Shows `content` (lines of text, or a table) in one of the extension's panels, with an optional title. | |
| `open-file(path, line)` | Opens a file in a tab, at a 1-based line. | `workspace-files` for project files |
| `notify(tone, message, detail)` | A notification: `success`, `error`, or info for the other tones. | |
| `ask-input(prompt, initial, secret)` | Asks for a line of text; `secret` makes it a password field. None when cancelled. | |
| `confirm(message, detail, action)` | Asks to confirm; `action` names the button. | |
| `settings()` | The extension's entry in settings.json, `"extensions": { "<id>": { … } }`, as JSON (`{}` when unset). | |
| `project-root()` | The open project's folder, if any. | |
| `secret-get(key)`, `secret-set(key, value)` | A value in the system keychain, under the extension's own name; `secret-set` with none removes it. | |
| `set-diagnostics(path, list)` | Replaces the extension's problems for a file: underlined in the editor and listed in Problems. 1-based line and column. | |
| `run-process(program, args, cwd)` | Runs a program and waits for it (up to 10 minutes): exit code, output, errors. | `process`, trusted project |
| `run-in-terminal(title, command)` | Runs a shell command in a terminal tab the user sees. | `process`, trusted project |
| `http-request(method, url, headers, body)` | An HTTP(S) request: status, headers, body. | `network` |
| `read-project-file(path)` | Reads a file in the open project (up to 16 MB), by a path relative to the project or absolute in it. | `workspace-files` |

Calls that wait for the user (`ask-input`, `confirm`, `pick`, a permission) block only the extension's own thread.

### Interface 0.2

With `api = "0.2"` the code is built against
[`kuana-wit/v0.2/kuana.wit`](../src/extensions/kuana-wit/v0.2/kuana.wit) and has these calls too: [`examples/extensions/text-tools`](../examples/extensions/text-tools)
is a small extension that uses them: it transforms the selection and counts words in the status bar.

| Call | What it does | Needs |
| --- | --- | --- |
| `editor(path, with-text)` | An open file's tab: `path`'s, or without one the tab the user is in. Its path, language, version, whether it's unsaved, its carets and selections (the main one first) and, when asked, its text. None when there's no such tab or the code can't reach the file. | `workspace-files` for project files |
| `apply-edits(path, version, edits)` | Edits the open file as one undo step and answers with the new version. | `workspace-write` for project files |
| `set-selections(path, version, selections)` | Sets the carets and selections, and scrolls to the main one. | `workspace-files` for project files |
| `write-project-file(path, content)` | Writes a file in the project (up to 16 MB), making its folders. An open tab of it reloads as after any change on disk; Local History keeps the version before. | `workspace-write` |
| `pick(placeholder, items)` | A list to pick from, narrowed by typing, like Find Action. The picked item's id, or none. | |
| `set-status(item, status)` | What a `[kuana.status.<id>]` item shows in this window: text (up to 40 characters), an icon, a tone, a tooltip. None hides it. | |
| `set-timer(name, seconds, repeat)`, `clear-timer(name)` | Tells `on-event` in `seconds` (1 at least), again and again with `repeat`. | |
| `spawn-process(name, program, args, cwd, panel)` | Starts a program and comes back at once. Its output goes to `panel` as it comes, when one is named. | `process`, trusted project |
| `kill-process(name)` | Ends a program `spawn-process` started, and what it started. | |
| `append-panel(panel, lines)` | Adds lines to the end of a panel's text, where `show-panel` replaces it. | |

**Places in text** are spans of UTF-8 byte offsets (`end` not included), so in Rust `&text[start..end]` is the
selection. An edit replaces its span with its text; an empty span inserts. Edits can't overlap and can't cut a
character in two.

**Versions of the text.** `editor` gives the text's version, and `apply-edits` and `set-selections` take the one
they were worked out for. Your code runs beside the editor, so the user may have typed in between: then the call
fails saying the text has changed, and nothing is edited. Read the editor again and retry.

**Timers** belong to the window that set them and end with it. One that comes due while the window isn't the
active one waits until it is, and the next wait starts only once your code has answered, so a slow tick never piles
up. A timer whose `on-event` fails stops.

**Programs that keep running.** `run-process` waits for a program and hands back all it printed; `spawn-process`
is for one you follow: a log, a build, a watcher. Each has a name of yours (one program per name, four at a time).
Its lines reach the panel a few times a second however fast it prints, the panel keeps the last 10,000 and stays at
the end unless the user scrolled up. With `"process-output"` in `events`, `on-event` gets the same lines in batches,
its `data` being `{"name": "…", "lines": ["…"]}`; when the program ends it gets `process-exited` with
`{"name": "…", "status": 0}` (-1 when it was ended). Kuana ends the program with its window, with the extension,
and when it quits.

### Events

0.2 code exports `on-event(event)` and lists what it wants in the manifest:

```toml
[kuana]
api = "0.2"
permissions = ["workspace-files"]
events = ["file-saved", "editor-changed"]
```

| Event | When | `path` | `language` |
| --- | --- | --- | --- |
| `project-opened` | A project opens in a window; also for the one already open when the extension is installed | its folder | |
| `project-closed` | The project closes or another replaces it | its folder | |
| `file-opened` | A file is opened in a tab and read | the file | yes |
| `file-closed` | Its last tab closes | the file | yes |
| `file-saved` | An explicit Save (not auto-save) | the file | yes |
| `editor-changed` | The user moved to another file's tab and stayed 200 ms | the file | yes |
| `settings-changed` | The extension's own settings changed | | |
| `timer` | A timer is due (never listed); its name is in `data` | | |
| `process-output` | A program started with `spawn-process` printed lines; `data` is JSON | | |
| `process-exited` | Such a program ended (never listed); `data` is JSON | | |

File events come only for files the code can reach: ones in its own folder, and project files when it has
`workspace-files` (the user is asked before the first one). A save of a file in the extension's own folder always
arrives, listed or not. There are no events for typing or moving the caret: a command asks for the selection when it
runs. An error from `on-event` shows as a notification.

Kuana doesn't load an extension's code while it starts: the first events are handed over two seconds after the
first window opens, in order.

### Files and paths

- The code's current directory is its own folder, `~/.config/kuana/extensions/work/<id>/`, created for it and removed
  when the extension is uninstalled. `std::fs` works there and nowhere else.
- In host calls, a relative path is in that folder. An absolute one must be in that folder or in the open project
  (which needs `workspace-files`). Nothing outside either is reachable, `..` and links included.

### Permissions

The code can only use what the manifest declares, and Kuana asks the user the first time it does:

| Permission | Lets the code |
| --- | --- |
| `process` | run programs (`run-process`, `run-in-terminal`), only for trusted projects or with none open |
| `network` | make HTTP requests (`http-request`) |
| `workspace-files` | read the open project's files and open them (`read-project-file`, project paths in `open-file`, `editor`, file events) |
| `workspace-write` | change the open project's files (`write-project-file`, `apply-edits` on a project file); 0.2 |

An allowed permission is remembered; a refused one is asked again after Kuana restarts. Settings → Extensions lists an
extension's permissions with a **Revoke** link. A call without its permission returns an error saying so.

### Limits

| Limit | Value |
| --- | --- |
| `children` | 15 s |
| `run-command` | 10 minutes (it may wait for the user) |
| `file-saved`, `on-event` | 2 minutes |
| A program it runs (`run-process`) | 10 minutes |
| Programs left running (`spawn-process`) | 4 per extension, no time limit |
| Computation per call | about 10 billion WebAssembly instructions |
| Memory | 512 MB |

Past a time limit Kuana stops waiting and starts the code afresh for the next call. Each extension's code runs on a
thread of its own, so a slow extension doesn't hold up Kuana or other extensions.

### Settings

Users configure an extension in settings.json; the code reads its entry with `settings()`:

```json
{
  "extensions": {
    "notes": { "folder": "~/Notes" }
  }
}
```

Declare the settings in the manifest and Kuana knows them: `settings()` then has each default for a key the user
hasn't set, settings.json completes and checks them, and the extension's row in Settings → Extensions lists them
with a control each: a switch for a boolean, a text field for a string or number (a number field saves only a
number), a dropdown for an enum. The title is the label and the description the line under it; Settings' search
finds them too. A change is written to `extensions.<id>.<key>` at once (the file's comments kept) and the code hears
`settings-changed`; picking the default, or emptying a field, removes the key, and the ↺ button beside a set one
resets it.

```toml
[kuana.settings.folder]
title = "Notes folder"
description = "Where notes are kept."
type = "string"            # string (the default), boolean, number or enum
default = "~/Notes"

[kuana.settings.order]
title = "Sort by"
type = "enum"
values = ["name", "date"]
default = "name"
```

Keys you don't declare still reach `settings()`; document those in your README.

### Key bindings

Commands and views can be bound in `keymap.json`:

```json
{
  "bindings": {
    "cmd-alt-n": ["workspace::RunExtensionCommand", { "extension": "notes", "command": "new" }],
    "cmd-alt-shift-n": ["workspace::ToggleExtensionView", { "view": "notes/notes" }]
  }
}
```

`RunExtensionCommand` also takes a `target`, passed to the command as is.

A command can come with a shortcut of its own:

```toml
[kuana.commands.new]
title = "New Note"
key = "secondary-alt-n"    # secondary is ⌘ on macOS, Ctrl elsewhere
```

Kuana binds it unless the user's keymap already uses that key anywhere: then it stays Kuana's, and the extension's
row in Settings → Extensions says which shortcut wasn't bound. What the user binds in `keymap.json` always wins.

### Sharing your extension

- **From a folder:** Settings → Extensions → Install from Folder….
- **From Git:** Settings → Extensions, paste the repository's URL. Kuana clones it and doesn't build anything, so
  commit `kuana.wasm` to the repository.
- **Kuana's registry** ([kuana-extensions](https://github.com/berkayakin/kuana-extensions)) builds extensions from
  their source: `kuana.wasm` from the crate in a `kuana/` folder, or from the root crate when that isn't Zed code.
  Extensions with views are listed once a Kuana release that runs them is out.

### Versions

The interface is versioned: `[kuana] api = "0.1"` is `kuana:extension@0.1.0`
([`kuana-wit/kuana.wit`](../src/extensions/kuana-wit/kuana.wit)), `"0.2"` is `kuana:extension@0.2.0`
([`kuana-wit/v0.2/kuana.wit`](../src/extensions/kuana-wit/v0.2/kuana.wit)). Kuana runs both, and new versions keep
running code built for older ones. 0.2 has everything 0.1 has, with `on-event` in place of `file-saved`. What only
the manifest declares (menu places, `key`, `[kuana.settings]`) works with either.
