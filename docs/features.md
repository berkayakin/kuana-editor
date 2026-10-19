# Features

Everything Kuana does, in one list. Each area has its own page with the details.

- Rope-backed buffer (`ropey`) with undo/redo; only visible lines are shaped and painted
- Tree-sitter syntax highlighting for JavaScript/JSX, TypeScript/TSX, HTML (including `<script>` blocks),
  CSS, Rust, Go, Python, PHP, shell scripts, JSON, YAML, TOML and Markdown; any other language
  through [extensions](extensions.md), which use Zed's format (Zed's language and theme extensions work);
  popular ones install from **Settings → Extensions → Available**
- Linting off the main thread:
  - **JavaScript** — syntax errors plus `no-unused-vars`, `no-undef`, `no-const-assign`,
    `no-dupe-keys`, `eqeqeq`, `use-isnan`, `no-cond-assign`, `no-debugger`, `no-var` (built on oxc)
  - **TypeScript** — the same rules except `no-undef` (TypeScript checks names itself); type-only
    uses count as uses; `.d.ts` files get syntax errors only. With a TypeScript language server
    running for the file, these rules step aside and only the server's problems show, as in Zed
  - **HTML** — unclosed/stray/void closing tags, duplicate attributes and ids, `img` alt,
    `lang`, doctype, `<title>`, obsolete tags, `target="_blank"` without `rel`; inline scripts are linted as JS
  - **Markdown** — the markdownlint rules (MD001–MD07x: heading structure, list style, code
    fence languages, trailing spaces, duplicate headings, broken reference and relative links…)
    via [rumdl](https://rumdl.dev). A project's `.markdownlint.json`/`.yaml`, `.rumdl.toml` or
    `pyproject.toml` `[tool.rumdl]` is used when present; otherwise line length (MD013) is off
  - **ESLint** — if the file's project has `node_modules/.bin/eslint`, it runs on open/save with the
    project's own config (JavaScript and TypeScript); built-in rules then drop back to syntax errors only
  - **CSS, Rust, Go, Python, PHP, shell, JSON, YAML, TOML** — syntax errors from the tree-sitter parser
    (unexpected and missing tokens); extension languages too when their `syntax_errors` setting is on
- Language servers (LSP) for code intelligence: diagnostics, completion, hover documentation,
  signature help, Go to Declaration / Implementation / Type Definition (on a declaration, ⌘B and
  ⌘/Ctrl-click list its usages, as in JetBrains; holding ⌘/Ctrl over a symbol the server can
  take you to underlines it and shows a pointing hand), Find Usages (grouped by file in a Find
  panel you step through with the arrows, pinnable per search), Call and Type Hierarchy there as
  trees that load as you open them, highlighted
  uses of the symbol at the caret, Go to Symbol across the project, Rename and context actions
  (quick fixes and refactorings); see [Language servers](languages.md#language-servers)
- Inlay hints from the language server (types, parameter names), rulers, visible whitespace,
  control characters shown as dim symbols (␌ for a form feed, ␛, NEL, LS for U+2028),
  column (box) selection with Alt+drag, sort/reverse/de-duplicate lines and case conversions
  (upper, lower, title, snake, kebab, camel, Pascal) from Find Action
- Vim mode (`vim_mode`): Normal, Insert, Visual and Visual Line; see [Vim mode](editing.md#vim-mode)
- Reformat Code (⌥⌘L / Ctrl+Alt+L) and optional format on save, with the project's formatter
  (e.g. its Prettier), the language server, or rustfmt/gofmt/ruff/black/taplo/shfmt on PATH
- Inline diagnostics, gutter markers, scrollbar markers, a Problems panel and Next / Previous Problem
- Multi-cursor (next/all occurrences, of the caret's word as a whole word with its case; ⌥-click,
  add caret above/below), syntax-aware Extend Selection,
  auto-closing pairs (not in strings and comments; typing steps over only the closing characters
  they put in), smart indent (closing brackets and `else:`/`fi` go back out as they're typed, pasted
  lines keep their shape at the caret's indentation), line/block comments, move/duplicate/join/delete lines;
  in an embedded language (JavaScript in `<script>`, a Markdown code fence) these follow that language
- Matching-bracket highlight (ignores brackets in strings and comments) and an editor right-click menu
- Soft wrap that keeps indentation, on by default for Markdown and plain text (View → Soft-Wrap
  toggles it per file)
- Markdown preview: the button at the top right of a Markdown file (or **Toggle Markdown Preview**,
  ⌘K V / Ctrl+K V in the VS Code and Zed keymaps) opens the rendered document in a tab of its own
  beside the text (a new split, or the next pane), updated as you type; the same button or shortcut
  closes it again. It can be moved, pinned and split like any tab and comes back with the project: headings, emphasis, lists and task lists, quotes, tables, highlighted code
  blocks, local images; links open in the browser. The HTML READMEs use is drawn too: centered
  blocks (`align="center"`), `<img>` with its width, headings, `<b>`/`<i>`/`<code>`/`<kbd>`, links,
  `<br>`, `<details>` (shown open) and tables; comments are hidden and other tags dropped with their
  text kept. Remote images (badges) show their alt text. Zoom it with the − / + control at its top right
  (click the percentage to reset) or ⌘/Ctrl + scroll
- Find/Replace bar with case, whole-word and regex options; Find/Replace in Files with preview
  (searches unsaved buffers too, honors .gitignore)
- Search Everywhere (double Shift, or the magnifier at the top right), fuzzy Go to File
  (git-ignored files included, ignored folders skipped), Recent Files, Find Action, Go to Line,
  and File Structure: the current file's functions, classes, headings, selectors or keys
- Breadcrumbs under the tabs (folders, file, then the definitions around the caret; click one to go
  there) and sticky scroll: the header lines of the functions and classes you've scrolled into stay
  pinned at the top of the editor (up to four; click one to jump to it)
- Git: clone, init, gutter change markers with rollback, inline blame, file status colors, a Commit panel
  (stage, discard, commit, amend, push), staging single changes, side-by-side diffs with changed words
  highlighted, a log with a branch graph and file history, merge/rebase with conflict resolution in
  the editor, branches, pull and fetch
- Welcome screen with quick-start tiles, Recent Projects, a Git card for the open repository and quick actions
- 50 built-in themes (36 dark, 14 light) and JSON user themes with live preview
- Multiple windows, one project each, with a Recent Projects picker; on macOS the
  Window menu and the Dock icon's menu list them. Recent projects are also in the Dock icon's
  menu (macOS) and the taskbar's jump list (Windows)
- Integrated terminal (⌥F12 / Alt+F12): your login shell in tabs, full colors, mouse support for
  full-screen programs, selection and copy/paste, Find in the output and scrollback; shell keys
  like Ctrl+R reach the shell; input methods (Chinese, Japanese, Korean) compose at the cursor;
  split groups, renamable tabs, and each project's terminals come back in their folders
- Code folding by indentation (Markdown by section), with gutter arrows and a `⋯` placeholder;
  folds are kept with the project
- Split panes (side by side or stacked, a file in both if you like) with draggable dividers; drag tabs to reorder them or
  move them between panes; pinned tabs; a tab menu with Close Others / All / to the Right
- Debugger for Rust, Go, Python and JavaScript/TypeScript (lldb-dap, Delve, debugpy, js-debug):
  breakpoints with conditions and logpoints, attaching to running processes, stepping, frames,
  variables, watches and an evaluating console; see [Debugging](run-and-debug.md#debugging)
- Restricted Mode for folders you haven't trusted (like VS Code's): no language servers,
  formatters or project tools until you do; see [Project trust](settings.md#project-trust)
- Python virtual environments detected per project and used by terminals, Run, the debugger,
  language servers and formatters; see [Python environments](languages.md#python-environments)
- Run and test: ▶ in the gutter next to tests and `main` functions (Rust, Go, Python, JS/TS),
  project commands (cargo, npm scripts, go, make, pytest) and your own run configurations, each
  in its own terminal tab with a pass/fail icon; test runs (cargo, go, pytest, Jest, Vitest, node) show
  their results as a tree in the Find panel and ✓ / ✕ in the gutter, with Rerun Failed; see
  [Run and test](run-and-debug.md#run-and-test)
- Image preview: PNG, JPEG, GIF (animated), WebP, BMP, TIFF, ICO, TGA and QOI files open as a
  picture with their dimensions and size (SVG stays text); Git diffs show an image's before and after
- Encodings and line endings: files keep their encoding (UTF-8 with or without BOM, UTF-16,
  Windows-125x, ISO-8859-2, Shift JIS, EUC-KR, GBK, Big5) and line endings when saved; the status
  bar shows both, and clicking them reloads the file in another encoding or converts it (as in
  JetBrains). A lone CR (old Mac files) reads as a line end and is saved as the file's line ending,
  as in Zed. Files that aren't UTF-8 are read as Windows-1252, so no byte is lost. Text an
  encoding can't hold is never saved in it. Files over 20 MB open as plain text without language
  features; over 500 MB they don't open. A file that looks binary (a NUL byte near its start, as Git
  tells) asks first, and opens as text only with Open Anyway
- Live file watching: the file tree, Go to File, Git status and branch, the Git log and open files
  follow changes made outside Kuana (a checkout, a generator, another editor) as they happen. In
  an open file, carets, the view, breakpoints, folds, problems and inlay hints stay with the lines
  they were on, Undo brings back the text from before, and language servers check the file again
  as after a save
- File tree with new file/folder, rename (language servers update imports and `mod` lines first), duplicate, copy/paste, Move to Trash, Find in Folder and
  Reveal in Finder; drag entries onto a folder to move them (open tabs follow); **Select Opened File**
  (⌥F1 / Alt+F1, or a tab's menu) shows the current file in the tree; ⌘Z / ⇧⌘Z (Ctrl+Z / Ctrl+Shift+Z)
  in the tree, or its right-click menu, undo and redo file operations, deletes included (they come
  back from the Trash; the notification after a delete has **Undo** too); tabs, custom title bar,
  IME support
- Local History: earlier versions of each file (on save, before and after outside changes, before a
  rollback), listed by day with a diff against the file as it is and Restore; see
  [Local History](editing.md#local-history)
- A Settings window for every option, with search across its pages; see [Settings](settings.md)
- Project settings in `.kuana/settings.json`, shared through the repository: indentation, formatters,
  language server settings and run configurations over each person's own; see
  [Project settings](settings.md#project-settings)
- Automatic updates from GitHub Releases (signed), installed with **Restart to Update**; see
  [Releases and updates](install.md#updates)
