# Docker: a sample Kuana extension

A Docker containers view on Kuana's activity bar, built with Kuana's extension UI API
(see [Writing view extensions](../../../docs/extension-ui.md)), interface 0.2:

- the containers on this machine, running ones first (green), each with its image, ports and status;
- a click shows its logs in a panel; right-click for Show Logs, Follow Logs (new lines arrive as the
  container prints them), Inspect (the JSON opens in a tab), Start and Stop (it asks first);
- Refresh, Show All as a Table and Stop Following Logs in the view's header;
- Docker Compose Up in a YAML file's right-click menu (it runs in a terminal tab).

It runs the `docker` command through Kuana, so it declares the `process` permission, narrowed to
`docker` alone (`[kuana.allow]`); Kuana asks before the first run, and refuses any other program.

## Build and install

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/kuana_docker.wasm kuana.wasm
kuana extension check .
```

Then **Settings → Extensions → Install from Folder…** and pick this folder.
