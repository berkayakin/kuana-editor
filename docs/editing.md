# Editing, panes and windows

## Panes and tabs

**Split Right / Down** (View menu, a tab's right-click menu, or ⌘\\ / ⌘⇧\\) opens a new pane
beside the current one showing the current file too: both panes edit the same text, each with its
own carets and scroll, and closing one keeps the file open in the other. Splitting the other way
than a pane's neighbors divides that pane, so panes make grids: split right, then split each side
down. Drag a tab to reorder it, onto another pane's tab bar or the middle of its editor to move it
there, or near a pane's edge to split that pane with the tab in the new half (the half lights up
while you drag); a pane closes with its last tab. Drag the gap between panes to resize them, or
double-click it to even them out. Panes keep a minimum size: one too small to halve doesn't split.

**Next Pane** / **Previous Pane** go through the panes left to right, top to bottom, and **Focus
Pane Left / Right / Above / Below** go to the pane on that side (⌘K ⌘← and so on in the VS Code
and Zed keymaps, Ctrl+K Ctrl+← off macOS).

Right-click a tab for Close, Close Others, Close All, Close Tabs to the Right, Pin Tab and the
split commands. Pinned tabs stay first, show a pin (click it to unpin) and are kept by Close
Others / Close All. The panes, as rows and columns, their tabs, sizes and pins come back with the
project.

## Zen mode

**Toggle Zen Mode** (View → Zen Mode, or Find Action) hides everything but the editor: the
activity bar, the sidebars, the panels, the status bar, the editor's tabs and breadcrumbs, the
minimap and the line numbers. The window goes full screen and a lone pane's text is centered, 100
characters wide; split panes keep the whole width. The VS Code keymap has it on ⌘K Z (Ctrl+K Z off
macOS) and Sublime Text's on ⌃⇧⌘F (Shift+F11); the JetBrains and Zed keymaps have no shortcut for
it, so give it one in Settings → Keymap if you like.

Move the pointer to the window's top for an **Exit Zen Mode** button, or toggle it again, to have
everything back as it was. A panel or the sidebar can still be opened while it's on (the terminal,
Problems, ⌘1); leaving Zen mode puts back the layout from before it. Quitting in Zen mode leaves
nothing hidden for the next launch either.

Settings → Appearance → Zen mode chooses whether it goes full screen (`zen_full_screen`), how wide
the text column is in characters (`zen_width`), and whether line numbers (`zen_line_numbers`) and
the editor's tabs (`zen_tabs`) stay.

## Vim mode

With `"vim_mode": true` (Settings → Editor → Vim), editors start in Normal mode with a block caret;
the status bar shows the mode and a command being typed.

- **Motions** with counts: `h j k l`, `w b e W B E`, `0 ^ $`, `gg G`, `{ }`, `%`, `f t F T` and `; ,`
- **Operators** `d c y > <` with a motion, a text object or doubled (`dd`, `cc`, `yy`, `>>`); text
  objects `iw aw iW aW`, `i" a" i' a'` (and backticks), `i( a( ib ab`, `i[ a[`, `i{ a{ iB aB`, `i< a<`
- `x X s S D C Y`, `r`, `~`, `J`, `p P`, `u` and ⌃R, `.` (repeats the last change, Insert text
  included), `i a I A o O`, `v V` (Visual: `d c y > < ~ u U J p o`), ⌃D / ⌃U half a page
- `/ ?` open the find bar, `n N` go to the next and previous match, `* #` search the word at the
  caret, `gd` goes to the declaration, and `:` takes `w`, `q`, `wq`, `x`, `wa`, `qa` or a line number

Yanks and deletes use the system clipboard. ⌘ shortcuts (Ctrl ones Vim doesn't use, elsewhere)
keep working in every mode, and an Insert session (with the operator that started it) is one undo
step.

## Snippets

Completions from a language server that contain placeholders (a function's arguments, a `for`
loop) select the first one; **Tab** and **Shift+Tab** move between them and **Escape** stops.
A placeholder used in several places is filled in everywhere at once.

Your own snippets use VS Code's format, so existing ones can be copied over. **Open Snippets
Folder** (Find Action) opens `snippets/` in the config folder, with an example on first use.
`<language id>.json` (`rust.json`, `typescript.json`, `typescriptreact.json`, `python.json`,
`shellscript.json`…) holds one language's snippets; `*.code-snippets` files hold any, limited
with `"scope": "rust,go"`. Type a snippet's prefix and press Tab, or pick it from completion.
Changes to the files apply right away.

Snippets fill in VS Code's variables: `TM_FILENAME`, `TM_FILENAME_BASE`, `TM_DIRECTORY`,
`TM_FILEPATH`, `TM_SELECTED_TEXT`, `TM_CURRENT_LINE`, `TM_CURRENT_WORD`, `TM_LINE_NUMBER`,
`CLIPBOARD`, the date and time (`CURRENT_YEAR`, `CURRENT_MONTH`, `CURRENT_DATE`, `CURRENT_HOUR`,
`CURRENT_DAY_NAME`… in local time), `LINE_COMMENT`, `BLOCK_COMMENT_START` / `_END`, `UUID`,
`RANDOM` and `RANDOM_HEX`; for the others, the default (`${WORKSPACE_NAME:app}`) goes in.

### Postfix completion

After an expression and a dot, completion lists postfix templates, as in JetBrains IDEs: `items.for`
becomes `for item in items { }`, `user.name.if` becomes `if user.name { }`, `x.not` becomes `!x`.
The expression is what ends at the dot on its line: names, calls, indexes and literals joined by
`.`, `::`, `->` or `?.` (`self.items[0].len()`, `read()?`). They're listed with the language
server's items (after them, for the same letters) and also without a language server.

| Language | Templates |
| --- | --- |
| Rust | `if`, `else`, `not`, `while`, `for`, `iter`, `match`, `let`, `letm`, `return`, `par`, `ref`, `refm`, `some`, `ok`, `err`, `box`, `dbg`, `println` |
| Python | `if`, `else`, `ifn`, `ifnn`, `not`, `while`, `for`, `enum`, `var`, `return`, `par`, `print`, `len`, `list`, `str` |
| JavaScript, TypeScript | `if`, `else`, `not`, `null`, `notnull`, `while`, `for`, `const`, `let`, `return`, `await`, `par`, `log`, `spread` |
| Go | `if`, `else`, `not`, `nil`, `nn`, `for`, `forr`, `var`, `return`, `par`, `print`, `len` |

An extension's language brings its own in a `[postfix]` table of its `config.toml` (see
[Extensions](extensions.md)).

## Edit predictions

Kuana can show code suggested after the caret as dimmed ghost text, from **GitHub Copilot**. It's
off until you choose it: Settings → Editor → Edit predictions → GitHub Copilot (or
`"edit_prediction_provider": "copilot"` in `settings.json`). Your code goes to GitHub only then.

- The status bar's **Install Copilot** installs GitHub's Copilot language server
  (`@github/copilot-language-server`, from npm, into Kuana's own folder; one on your PATH is used
  instead). It needs a GitHub account with a Copilot plan; the free plan works.
- **Sign in to Copilot** in the status bar (or **Copilot: Sign In** in Find Action) shows a code:
  **Copy Code and Open GitHub** copies it and opens GitHub's page, where you paste it and approve.
  The status bar shows the code until that's done, then **Copilot** with a green light. Copilot
  keeps the sign-in in its own folder (on macOS and Linux `~/.config/github-copilot`, or under
  `XDG_CONFIG_HOME` when that's set), so it holds for every window and the next launch. Click
  **Copilot** in the status bar to sign out or turn suggestions off (**Copilot: Sign Out** in Find
  Action does the first).
- Suggestions come a moment after you type: the first line at the caret, the rest as dimmed lines
  under it. **Tab** accepts one (an open completion list takes Tab first), **Escape** or moving the
  caret dismisses it, and typing what it suggests keeps the rest. **⌥\\** (Alt+\\ on Windows and
  Linux) asks for one at the caret.
- When a suggestion writes again what's after the caret, it goes after that text: with the caret in
  `def is_prime(n|)`, where typing `(` added the `)`, the suggestion shows (and goes in) after the
  `)`. Stepping over the `)`, by typing it or with →, keeps the suggestion.
- `"edit_predictions": false` turns them off for a language (under `"languages"`) or a project (its
  `.kuana/settings.json`). Environment files (`.env`, `.env.local`), keys and certificates (`.pem`,
  `.key`) and registry credentials (`.npmrc`, `.netrc`) never get suggestions, and in Restricted
  Mode nothing is sent.

Suggestions show with one caret and nothing selected, and in Vim mode only in Insert mode. On a line
that soft wrap breaks, nothing shows until the caret is on a line that fits.

For chat and agents, use the tools you already have: Claude Code, Codex or Gemini CLI run in
Kuana's terminal, and Kuana picks up the files they change (the Git gutter and the Commit view show
what they did, Local History undoes it). Claude Code goes further: it connects to Kuana, as below.

## Claude Code

[Claude Code](https://claude.com/claude-code) running in a terminal connects to Kuana the way it
connects to VS Code and JetBrains IDEs (its `/ide` command), so it works with what you see:

- **It sees your selection.** Claude knows which file you're in and what you've selected, so
  "this function" means the one you're looking at. **Add to Claude Context** (⌥⌘K / Ctrl+Alt+K in the JetBrains and Sublime
  Text keymaps; the editor's right-click menu; Find Action) puts the file, or its selected lines, in
  Claude's prompt.
- **It reads the problems Kuana shows**: language servers', linters' and Kuana's own, open files
  and the rest.
- **Its edits come as diffs.** When Claude asks to change a file (not in its auto-accept mode), the
  change opens in a **Proposed by Claude Code** tab beside the file: the file as it is, unsaved
  edits included, against Claude's version. **Accept** applies it as one edit you can undo and saves
  the file (Local History keeps what it was, "Before Claude Code's edit"); **Reject**, or closing
  the tab, turns it down. Answering in the terminal instead closes the tab.

Run `claude` in Kuana's terminal and it connects by itself. Started in another terminal, in the
project's folder, type `/ide` and pick Kuana (or start it with `claude --ide`). While it's
connected the status bar shows **Claude** with a green light. One Kuana window per project: each
window offers its own folders.

Nothing goes anywhere through Kuana: the connection is on your machine (a local port, with a lock
file in `~/.claude/ide` holding its key). It's on once Claude Code has run on your machine, for
local projects (not [remote ones](remote.md)); Settings → Editor → Claude Code
(`"claude_code_integration": false`) turns it off.

## Multibuffers

A multibuffer puts places from many files in one tab, each as an excerpt: a few lines around
the place, editable right there. **Open in Editor** (the page button in the Find panel's header,
or **Open Results in Editor** in Find Action) opens what the Find panel lists: usages from Find
Usages, the TODO list, test results. **Problems in Editor** (View menu, the Problems panel's
header, Find Action) does the same for the project's errors and warnings: open files', the language
servers' reports for files you haven't opened (rust-analyzer, for one, checks the whole project) and
the last Run's. It stays up to date while it's open: problems that come show up, files whose
problems are fixed go, and the excerpt you're typing in stays put until you leave the tab. In Find in Files, **open all in editor** (⌘↩ / Ctrl+Enter) turns its results into a
multibuffer with every match marked, so a search across the project can be edited in place.

- Each file has a header with its name and folder; clicking it, or **F4** in an excerpt, opens the
  file's tab at the caret.
- Excerpts show two lines around each place; nearby places share one. A **⋯ N lines** strip marks
  what's left out: clicking it shows up to five more lines, and excerpts that meet become one.
- ↑ and ↓ go from one excerpt into the next, as if the lines were one file.
- An excerpt is the file itself, not a copy: highlighting, problems, Git marks, completion,
  quick fixes, Rename and Undo work as in its tab, and an edit is at once in the file's tab too.
  **Save** saves every file the multibuffer changed; closing it keeps unsaved edits (the status bar
  lists them, as for files changed without a tab).
- Multiple carets stay within one excerpt.

## Semantic highlighting

Highlighting comes from tree-sitter, which knows the syntax but not what a name is. A language
server that sends semantic tokens (rust-analyzer, gopls, TypeScript's and Python's servers…) colors
over it what only it knows: a name that's a type, a constant, an enum member, a parameter or a
macro, and a library's own functions and types. A mutable variable is underlined, as in VS Code and
JetBrains IDEs. The theme's colors apply (`variable.parameter`, `function.macro`… fall back to their
base color); token kinds a theme has no color for keep tree-sitter's. Typing keeps the server's
colors where the text didn't change, and they're asked for again once you pause. Turn it off with
`"semantic_tokens": false` (Settings → Editor → Colors), per language too.

## Colors

In CSS, SCSS, Less, HTML and the like, each color in the text (`#e11d48`, `rgb(…)`, `rgba(…)`,
`hsl(…)`, `hsla(…)`) has a swatch before it. Clicking the swatch opens a picker: a
saturation/brightness field, a hue bar and an alpha bar. The color in the text changes as you drag,
written the way it was (hex stays hex, `rgb()` stays `rgb()`, with alpha added only when it isn't
opaque), and the whole change is one undo step. Escape or a click elsewhere closes the picker. A
`#name` in a selector isn't taken for a color.

A language server that finds colors (`colorProvider`: the CSS server, Tailwind's for `bg-red-500`)
supplies them instead of the scan, in any language. Its colors written in CSS notation can be
picked; others (a class name) show only the swatch. Turn swatches off with
`"document_colors": false` (Settings → Editor → Hints), per language too.

## Linked editing

Typing in a tag's name changes its closing tag with it, and the other way round: `<li>` → `<link>`
renames `</li>` → `</link>` as you type, in one undo step. It works in HTML, JSX and TSX (`<UI.Button>`
too), and in extension languages whose grammars name tags the same way (Vue, Svelte, XML); it needs
no language server. A space or `>` ends the name, leaving the other tag alone. Emptying a name and
typing a new one still renames both. Turn it off with `"linked_editing": false` (Settings → Editor →
Typing), per language too.

## Back and Forward

**Back** goes to where the caret was before the last navigation, and **Forward** undoes a Back, as
in a browser. Navigations are jumps: Go to Declaration (and Implementation, Type Definition), a
result in the Find panel, Find Usages or a picker (Go to File, Go to Symbol, File Structure), Go to
Line, bookmarks, a `file:line` link in the terminal, a sticky line, a breadcrumb or a problem, and
switching to another tab. Typing and moving the caret with keys or clicks aren't, so Back skips
them. A new navigation after Back drops what Forward had left, and places follow the edits made
since, files that are renamed or moved in Kuana, and files closed since (they open again).

| Keymap | Back / Forward on macOS | on Windows / Linux |
| --- | --- | --- |
| JetBrains | ⌘[ / ⌘], or ⌥⌘← / ⌥⌘→ | Ctrl+Alt+← / Ctrl+Alt+→ |
| VS Code | ⌃- / ⌃⇧- | Alt+← / Alt+→ |
| Sublime Text | ⌃- / ⌃⇧- | Alt+- / Alt+Shift+- |
| Zed | ⌃- / ⌃⇧- | Ctrl+Alt+- / Ctrl+Alt+Shift+- |

A mouse's back and forward buttons do the same. Both are at the top of the Navigate menu and in
Find Action (Navigate Back / Navigate Forward). The history is the window's, from the project's
opening; it isn't kept when Kuana quits.

## Bookmarks

A bookmark marks a line to come back to, in any file. **Toggle Bookmark** (F3 on macOS, F11 on
Windows and Linux) adds one on the caret's line, or on every caret's line, and removes them when they
all have one. A bookmarked line shows a bookmark in the gutter, where the fold arrows go (an arrow
wins while it shows), and a mark on the scrollbar. Bookmarks move with their lines as you edit and
go when their line is deleted.

- **Toggle Bookmark with Mnemonic…** (⌥F3 / Ctrl+F11) picks a digit or letter for the line: type
  it, or click it in the grid. Each mnemonic is used once per project, so picking one that's taken
  moves it. A bookmark with a mnemonic shows its letter in the gutter.
- **Go to Bookmark 0–9** (⌃0–⌃9 / Ctrl+0–Ctrl+9) jumps to the bookmark with that digit.
- **Edit Bookmark Description…** adds a note, shown in Show Bookmarks.
- **Show Bookmarks** (⌘F3 / Shift+F11) lists every bookmark in the project with its line;
  type to filter, Enter goes there, and ⌫ with nothing typed removes the selected one.
- **Next Bookmark** / **Previous Bookmark** go to the one after or before the caret, file by
  file, starting over at the end.
- Right-clicking a line number offers the same for that line (with the breakpoint options, in files
  a debugger can stop in).

These are in the Navigate menu and Find Action. With the VS Code or Zed keymap, Toggle Bookmark is
⌥⌘K / Ctrl+Alt+K and Next / Previous Bookmark ⌥⌘L / ⌥⌘J (Ctrl+Alt+L / J), as in VS Code's
Bookmarks extension; with Sublime Text's, they're ⌘F2 / Ctrl+F2, F2 and ⇧F2. Bookmarks are kept
with the project, for open and closed files alike, and follow a file that's renamed or moved in
Kuana.

## TODO list

**Show TODO** (View → TODO, or Find Action) lists the project's TODO items in the Find panel, as
JetBrains IDEs do: comments that contain one of the words in `todo_patterns` (TODO, FIXME, HACK and
XXX by default; in any case, as a whole word), grouped by file, each with the rest of its comment.
↑/↓ show each one in its file and Enter goes there. The buttons in the panel's header switch between
the whole project, the current file and the files with changes (Git's status).

Only comments count: a TODO in a string or in plain text isn't listed. Files are read the way Find in
Files reads them, so git-ignored files are skipped. The project is read the first time you open the
list; after that, files are read again as they change, on disk or in a tab (unsaved edits included),
and the list follows. The same words are highlighted in the editor, from the word to the comment's end.

## Local History

Kuana keeps earlier versions of your files, whatever Git knows, as JetBrains IDEs do: when you save
(and, the first time Kuana saves a file, what it was before), before and after a change from outside
replaces an open file (a checkout, a formatter, another editor), before a Git rollback and before a
Restore. **Local History** (Find Action, the editor's right-click menu, or a file's right-click
menu in the tree) lists a file's versions in the Find panel by day, each with its time, why it was
kept and how long ago. ↑/↓ show each one's differences from the file as it is (unsaved edits
included) in a diff tab; **Restore** in the diff puts that version back as one edit, which Undo
takes back.

Versions live in `~/.config/kuana/local_history/`, outside your projects. A version the same as
the one before isn't kept again, saves less than a minute apart keep only the last (auto-save),
and files over 2 MB have none. Versions go after 14 days, and a file keeps at most 200.

## Comparing files

Any file can be compared, in a diff tab like Git's, with:

- **the clipboard**: **Compare with Clipboard** (the editor's or a file's right-click menu, Find
  Action);
- **another file**: **Compare With…** picks it from the project's files (Go to File's list); in the
  file tree, **Compare with Editor** compares that file with the current tab's;
- **itself on a branch or tag**: **Compare with Branch…** lists the repository's branches and tags.

The file you compare from is the right side, as it is: unsaved edits show, and the diff follows as
you type. The other side is the clipboard as it was, the other file as it is on disk (or its unsaved
text, when its tab has edits), or Git's version.

## Scratch files

A scratch file is a throwaway file in any language, for trying something out apart from the
project. **New Scratch File…** (File menu, Find Action; Ctrl+Alt+Shift+Insert on Windows and Linux)
asks for the language (the current file's comes first) and opens an empty `scratch_<n>.<suffix>`.
It's an ordinary file tab, with highlighting, completion and auto-save, kept in
`~/.config/kuana/scratches/` between runs, outside any project. **Scratch Files…** lists them,
newest first; ⌫ there moves one to the Trash.

Python, JavaScript, TypeScript (Node 22.6 or later), shell, Go, PHP and Rust scratches have a ▶ on
their first line that runs the file in a Run tab (Rust compiles it with `rustc` first). A language
server may not know a file outside its project: it treats a scratch as a file of its own.

## Terminal shell integration

In zsh, bash and fish, Kuana's terminal knows where each command starts and how it ended:

- A dot left of each prompt whose command ran: gray when it succeeded, red when it failed, in the
  accent color while it runs. Marks at the terminal's right edge show every command in the
  scrollback the same way.
- **Previous Command** / **Next Command** (⌘↑ / ⌘↓ on macOS, Ctrl+↑ / Ctrl+↓ elsewhere, in the
  terminal) scroll the prompt before or after the top of the view to the top.
- **Copy Command Output** on the terminal's right-click menu copies the output of the command
  you clicked; **Terminal: Copy Last Command Output** (Find Action) the last one's.
- `file:line` links in the output are relative to the folder the shell is in now, and the folder a
  terminal comes back in with the project is the shell's.

Kuana starts these shells with a small script of its own that reads your usual files first (zsh:
through `ZDOTDIR`; bash: `--init-file`, reading the login profile itself; fish: `--init-command`),
then marks prompts with the OSC 133 and OSC 7 sequences VS Code and iTerm use. Other shells that
send those marks themselves work too. The scripts are in `<config>/shell-integration/`. Turn it
off with `"terminal_shell_integration": false` (Settings → Terminal); it applies to new terminals.
Not on Windows yet.

## Windows and projects

Each window holds one project, so several projects can be open side by side. A project is one
folder or several (see [Several folders in one window](#several-folders-in-one-window)).

- **Open Folder…** (⌥⌘O / Ctrl+Alt+O) asks whether to open the folder in a new window, in this
  window, or add it to the project (`open_folder_in` in Settings answers for you). A window with no
  project yet is reused.
- **Open Folder in This Window…** replaces the window's project. Its tabs, including unsaved
  edits, are kept for when you open it again.
- **Recent Projects…** (also opened by clicking the project name in the title bar) lists recent
  folders. Enter opens one in this window; ⌘↩ / Ctrl+Enter or ⌘/Ctrl-click opens it in a new
  window. The welcome screen's Recent Projects list works the same way.
- The same recent projects are in the Dock icon's right-click menu on macOS (also while Kuana
  isn't running) and in the taskbar icon's jump list on Windows. Choosing one opens it in Kuana,
  bringing forward a window that already has it.
- **New Window** (⌘⇧N) opens an empty window.
- Opening a folder that's already open in another window brings that window forward.
- Files and folders dragged from Finder go where you drop them: onto the editor area they open
  (a "Drop to open" frame shows while dragging); onto the file tree they're copied into the
  highlighted folder, and in a Git project Kuana offers to add them to Git. Kuana is listed in Finder's
  **Open With** for files and folders (the release app), in Explorer's **Open with** for text, docs
  and code files (the Windows installer; its optional **Open with Kuana** task adds folders' context
  menu too), and in Linux file managers' **Open With** for them (the desktop entry
  [`install.sh`](install.md#install-script-macos-linux) writes). It's offered there, never made the
  default app. **Install 'kuana' Shell Command** (Kuana menu
  or Find Action) adds `/usr/local/bin/kuana`, asking for an administrator password if needed:
  `kuana .` or `kuana src/main.rs` opens them in the running Kuana; `kuana api web` opens one
  project with both folders.
- Quitting or closing a project window doesn't ask about unsaved edits: they're kept and come back
  with the project's tabs. Windows without a project folder still ask.

These are all in the File menu and Find Action.

### Several folders in one window

A project can hold several folders, such as a front end and its API, or a library and the app
using it:

- **Add Folder to Project…** (File menu, Find Action, a folder row's right-click menu) adds
  folders; so does dropping folders from Finder onto the file tree's empty space, or
  `kuana api web` on the command line. **Remove Folder from Project** is on a folder row's menu.
  The first folder is the project's own: it names the window and can't be removed.
- The file tree shows each folder as a row of its own, closed and opened like a folder.
- Go to File, Search Everywhere, Find in Files, TODO, the Test Explorer and Problems cover every
  folder, and paths show with their folder's name in front (`web/src/app.ts`). Find in Files can
  search all folders or one, with the folder chips at its top.
- Each folder keeps its own language servers, project settings (`.kuana/settings.json`) and trust:
  a folder that isn't trusted is asked about when it's added, and Restricted Mode shows until it is.
- The project remembers its folders: reopening its first folder, from Recent Projects or at startup,
  brings back all of them with the tabs. Recent Projects names it by all its folders (`api, web`).
- Folders are all on one machine: a remote project's Add Folder… asks for a folder on that machine.
- Extensions see every folder: their project files are in any of them (`project-root` is the first).
- Each folder's Git repository is its own: the Commit panel shows the current file's, and its
  repository button picks another (see [Several repositories](git.md)).
- Run… and Debug… list each folder's commands and debug configurations, named after their folder
  (`web: npm run test`); the settings' run configurations are the current file's folder's. The Test
  Explorer groups tests by folder, each with Run for its folder's test command, and Run All runs
  them all. A new terminal starts in the current file's folder; each folder keeps its own Python
  venv and project settings (Open Project Settings opens the current file's folder's).

## Remembered state

Kuana reopens the last project folder when started without arguments, lists the last ten
folders under Recent Projects (shared by all windows; hover a welcome-screen row to remove it), and remembers
panel sizes, whether the sidebar, Problems panel, terminal and Git log are open (all but the sidebar start
closed the first time; without a project the sidebar is always closed), which sidebar view (Files or Commit) shows, the font zoom, the diff layout and the window's size and
position (on the same display while it's connected, full screen included). This lives in
`state.json` next to `settings.json` in the config folder.

Each project also reopens with the tabs it had, the active tab, each tab's caret and scroll
position (a split of one file keeps both), the folders expanded in the file tree, breakpoints,
bookmarks and the last run configuration. Unsaved edits
are kept as well (like VS Code's hot exit) and undo goes back to the file's saved text. These
live in `sessions/` in the config folder, one file per project.
