# Terminal, run and debug

## Terminal

**Terminal** (⌥F12 / Alt+F12, or its button at the top right) opens a shell in the project folder in
the bottom panel, which it shares with Problems. Press it again to jump between the terminal and
the editor. Each **+** tab is its own shell; a tab closes when its shell exits.

While the terminal has focus, keys go to the shell: Ctrl shortcuts (Ctrl+R history search, Ctrl+F,
Ctrl+E…) aren't taken by Kuana, except a few for getting around (⌥F12, tab switching, Find Action).
On macOS, Option types characters as usual (`@` on many layouts). Full-screen programs (vim,
htop, less) get mouse and wheel events; hold Shift to select text in them instead. Colors come
from the theme's `terminal_*` keys.

Hold ⌘ (Ctrl on Windows/Linux) over a URL or a file path in the output, such as a compiler's
`src/main.rs:12:5`, to underline it, and click to open it: URLs in the browser, files in the editor
at that line and column. Paths are relative to the terminal's folder and must exist. The split
button above the terminal (or **Split Terminal** in Find Action) opens a shell in a new group beside
the current one, with its own tabs; a group goes away with its last tab.

Right-click a terminal tab to **Rename** it (empty goes back to the shell's own title), clear it,
open a new or split terminal, or close it, the others, the tabs to its right or all of its group's
tabs; Run tabs also get **Stop** or **Rerun**. Each project remembers its terminals: how many,
their names, which are split into groups, the folder each shell was in (after a `cd`) and whether
the panel was open. They start again when the project opens (or when the panel first opens).
Run tabs aren't brought back.

**Find** (⌘F / Ctrl+Shift+F in the terminal; the selection becomes the query) highlights matches in
the scrollback and on screen and jumps to the newest; ↵ or ↑ goes to older ones, ⇧↵ or ↓ to newer,
Esc closes it. Matching ignores case unless the query has capitals.

## Run and test

A ▶ in the gutter marks what can run in the file; click it to run that one thing:

| Language | ▶ on | Runs with |
| --- | --- | --- |
| Rust | `#[test]` functions, `#[cfg(test)]` modules, `fn main` | `cargo test … --exact`, `cargo run` (with `--bin`/`--example`/`--test` as needed) |
| Go | `Test…`, `Benchmark…`, `Example…` functions, `func main` | `go test -v -run '^Name$'`, `go run .` |
| Python | `test_…` functions, `Test…` classes and methods, `if __name__ == "__main__"` | `python3 -m pytest -v file::Class::test`, `python3 file` |
| JavaScript, TypeScript | `test(…)`, `it(…)`, `describe(…)` | the project's vitest or jest, else `node --test` |

**Run at caret** (⌃⇧R / Ctrl+Shift+F10) runs the innermost test or function around the caret.
**Run…** (⌃⌥R / Alt+Shift+F10) lists the last run first, then your `run_configurations`, the
project's commands (cargo run/test/build and binaries, `package.json` scripts with its package
manager, go, Makefile targets, pytest) and the current file's tests. **Run** (⌃R / Shift+F10)
repeats the last one (run or debugged), **Stop** (⌘F2 / Ctrl+F2) interrupts it. The title bar's
top right has the same: the last configuration (click it for Run…; the project keeps it for next
time), Run, Debug and, while something runs, Stop, next to the sidebar and bottom panel toggles.

Each run gets a terminal tab named after it, which stays open with the exit code when it ends and
shows ▶ while running, ✓ on success and ✕ on failure. Running the same thing again reuses its tab.
Unsaved files are saved first. Add your own commands in `settings.json`:

```jsonc
"run_configurations": [
  { "name": "Dev server", "command": "npm run dev -- --port 3000" },
  { "name": "API tests", "command": "pytest -x tests/api", "cwd": "backend" }
]
```

`cwd` is relative to the project folder. Commands run through your login shell, so aliases and
PATH work as in the terminal.

### Test results

When a test run ends (cargo test, `go test -v`, pytest, Jest, Vitest or `node --test`, from the
gutter, Run… or your own configurations), Kuana reads its results from the output:

- A notification says how it went ("Tests failed: 1 failed, 12 passed"), with **Show Results**.
- The **Find** panel gets a tab for the run: the tests in a tree by module, file and class (Go's
  subtests under their test), each with ✓, ✕ or a dash for skipped ones, and every group marked
  with its tests' worst. A failed test is open, showing what it printed (the panic or assertion),
  and goes to the line where it failed. **Rerun Failed** runs only the failed tests the same way
  (`cargo test -- a b --exact`, `go test -run '^(A|B)$'`, pytest node ids, `-t` for Jest and
  Vitest, `--test-name-pattern` for node); **Rerun** runs it all again. A new run replaces the tab
  unless you pin it.
- The gutter's ▶ next to a test that ran turns into its result, ✓ or ✕ (a module, class or test
  with subtests shows ✕ if any test under it failed); clicking it still runs it.

**Rerun Failed Tests** (Find Action) does the same for the last test run from anywhere.

When a run ends, the errors and warnings in its output go to **Problems** (under the run's name,
below the current file's own), each opening its file at the line: rustc and cargo messages, Rust
panics, `tsc`, Python tracebacks, Node stack traces, and the common `file:line:col: message` of Go,
gcc/clang and linters. The status bar's counts include them. The next run replaces them; one that
reports nothing clears them.

## Debugging

Rust, Go, Python, JavaScript and TypeScript tests and programs can run under a debugger, through
the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/) (the one VS Code
and Zed use):

- **Breakpoints**: click a line number (it becomes a red dot) or use **Toggle Line Breakpoint**
  (⌘F8 / Ctrl+F8; F9 in the VS Code and Zed keymaps). They move with their lines as you edit and are
  kept with the project.
- **Conditions and logpoints**: right-click a line number for **Add Conditional Breakpoint…**
  (stops only when an expression is true, e.g. `count > 10`; ⇧⌘F8 / Ctrl+Shift+F8 on the caret's
  line) and **Add Logpoint…** (prints a message instead of stopping; `{expression}` shows a value,
  e.g. `count = {count}`). A conditional breakpoint has `=` in its dot, a logpoint is a diamond;
  right-click it again to edit or remove it. An empty text clears the condition or message.
- **Start**: click a gutter ▶ and pick **Debug**, use **Debug at Caret** (⌃⇧D / Ctrl+Shift+F9) or
  **Debug…** (⌃⌥D / Alt+Shift+F9) for the project's configurations, or the Run menu. Unsaved files
  are saved first; Rust is built with cargo (its output shows in the console). JavaScript and
  TypeScript debug what Run would run (`node`, `npx tsx`, npm/pnpm/yarn scripts, Vitest, Jest,
  `node --test`), every Node process it starts included; TypeScript breakpoints work through source maps.
- **The project's own configurations**: Debug… also lists what the project wrote for VS Code
  (`.vscode/launch.json`) or Zed (`.zed/debug.json`), with their variables filled in
  (`${workspaceFolder}`, `${file}`, `${env:NAME}`, Zed's `$ZED_WORKTREE_ROOT`…). Kuana starts the
  adapter they name (`node`/`pwa-node`, `debugpy`/`python`, `go`/`Delve`, `lldb`/`CodeLLDB`, or an
  extension's) with the configuration as written; `preLaunchTask` isn't run.
- **Attach to Process…** (⌥⇧F5 / Ctrl+Alt+F5, or the Run menu) lists your running processes (search
  by name, command line or PID) and debugs the one you pick: Node with js-debug, Python with debugpy,
  Go with Delve, anything else (Rust, C, C++) with lldb-dap. Stop detaches and leaves it running.
- **The Debug panel** (bottom area; the bug button at the top right brings it back) shows the
  stepping toolbar, the paused thread's frames (click one to see its line and variables), variables
  you can expand, watch expressions, and a console with the program's output that also evaluates
  expressions in the selected frame. The line the program is paused on is highlighted.
- **Stepping**: Resume ⌥⌘R / F9, Step Over F8, Step Into F7, Step Out ⇧F8, Stop ⌘F2 (in the VS Code
  and Zed keymaps: F5, F10, F11, ⇧F11, ⇧F5, and Pause F6). F5 also starts debugging again.

Nothing is downloaded; the adapters come from your system:

| Language | Adapter | Install |
| --- | --- | --- |
| Rust | `lldb-dap` (with Rust's own formatters, so `String`, `Vec` and `Option` read naturally) | Xcode's command line tools (`xcode-select --install`) or LLVM |
| Go | Delve (`dlv dap`) | `go install github.com/go-delve/delve/cmd/dlv@latest` |
| Python | debugpy, in the project's venv or `python3` | `python3 -m pip install debugpy` |
| JavaScript, TypeScript | Microsoft's js-debug (the one in VS Code), run with `node` | Unpack `js-debug-dap-v*.tar.gz` from [its releases](https://github.com/microsoft/vscode-js-debug/releases) into `~/.config/kuana/debug-adapters` (so `debug-adapters/js-debug/src/dapDebugServer.js` exists), or put `js-debug-adapter` on PATH |

One session runs at a time. Attaching to Python injects debugpy into the process, which macOS can
block; attaching to Node turns on its inspector (port 9229 unless it was started with `--inspect`).
An extension can bring another adapter (`[debug_adapters]`, see [Extensions](extensions.md)).
