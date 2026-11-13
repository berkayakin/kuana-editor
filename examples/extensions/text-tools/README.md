# Text Tools: a sample Kuana extension for interface 0.2

Where the [Docker sample](../docker) adds a view (interface 0.1), this one changes the open file and
follows what you do, with what [interface 0.2](../../../docs/extension-ui.md#interface-02) adds:

- **Transform Selection…** (the editor's right-click menu, Find Action, `Ctrl+Alt+Shift+T`) asks which
  way in a pick list, then rewrites every selection as one undo step: `editor`, `pick`, `apply-edits`;
- a **status bar item** counts the words of the file you're in, kept current by the `editor-changed`,
  `file-saved` and `settings-changed` events: `set-status`, `on-event`;
- **Show Text Statistics** (a tab's right-click menu, or a click on the count) fills a panel with a table;
- a **setting** with a default, `"extensions": { "text-tools": { "count": "characters" } }`.

It reads and edits project files, so it declares `workspace-files` and `workspace-write`; Kuana asks
before the first use of each.

## Build and install

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/kuana_text_tools.wasm kuana.wasm
kuana extension check .
```

Then **Settings → Extensions → Install from Folder…** and pick this folder.
