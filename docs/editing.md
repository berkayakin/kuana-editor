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

## Windows and projects

Each window holds one project folder, so several projects can be open side by side.

- **Open Folder…** (⌥⌘O / Ctrl+Alt+O) opens the folder in a new window. A window with no
  project yet is reused instead.
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
  **Open With** for files and folders (the release app). **Install 'kuana' Shell Command** (Kuana menu
  or Find Action) adds `/usr/local/bin/kuana`, asking for an administrator password if needed:
  `kuana .` or `kuana src/main.rs` opens them in the running Kuana.
- Quitting or closing a project window doesn't ask about unsaved edits: they're kept and come back
  with the project's tabs. Windows without a project folder still ask.

These are all in the File menu and Find Action.

## Remembered state

Kuana reopens the last project folder when started without arguments, lists the last ten
folders under Recent Projects (shared by all windows; hover a welcome-screen row to remove it), and remembers
panel sizes, whether the sidebar, Problems panel, terminal and Git log are open (all but the sidebar start
closed the first time; without a project the sidebar is always closed), which sidebar view (Files or Commit) shows, the font zoom, the diff layout and the window's size and
position (on the same display while it's connected, full screen included). This lives in
`state.json` next to `settings.json` in the config folder.

Each project also reopens with the tabs it had, the active tab, each tab's caret and scroll
position (a split of one file keeps both), the folders expanded in the file tree and the last
run configuration. Unsaved edits
are kept as well (like VS Code's hot exit) and undo goes back to the file's saved text. These
live in `sessions/` in the config folder, one file per project.
