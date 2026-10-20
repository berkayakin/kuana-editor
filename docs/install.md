# Installing and building

## Install script (macOS, Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/berkayakin/kuana-editor/main/install.sh | sh
```

[`install.sh`](../install.sh) downloads the latest release, checks it against the SHA-256 in the
release's `latest.json` and installs it:

- **macOS:** `Kuana.app` in `/Applications` (`~/Applications` when that isn't writable). macOS
  doesn't quarantine what `curl` downloads, so the first launch doesn't need
  [Open Anyway](#first-launch-on-macos).
- **Linux:** the `kuana` binary in `~/.local/bin`, plus a desktop entry and icons in
  `~/.local/share`, so app launchers list it.

It then opens Kuana. Run it again to reinstall. `KUANA_INSTALL_DIR=<folder>` installs somewhere
else; `KUANA_NO_LAUNCH=1` doesn't open it.

## Download

Get the latest build from [GitHub Releases](https://github.com/berkayakin/kuana-editor/releases/latest):

- **macOS** (Apple silicon and Intel): `Kuana-<version>-macos.dmg`, or the `.zip`
- **Windows** (x64 or Arm64: `x86_64` or `aarch64` in the file name): the `-setup.exe` installer
  (per user, no admin rights needed), or the `.zip`
- **Linux** (x64 or Arm64): the `.tar.gz`

macOS is where Kuana is used every day. Windows and Linux builds work but have had much less
testing, so [bug reports](https://github.com/berkayakin/kuana-editor/issues) are very welcome.

## Building from source

```bash
git clone https://github.com/berkayakin/kuana-editor.git
cd kuana-editor
cargo run --release -- path/to/folder path/to/file.js
```

Each folder on the command line opens in its own window; files open in the first one.

GPUI's `runtime_shaders` feature is enabled, so no separate Metal toolchain is needed on macOS.
Building needs `cmake` (for wasmtime, which runs extensions' WebAssembly grammars and code):
`brew install cmake` on macOS.

### App icon

The icon (a purple serif K with a pastel painted peacock) lives in [`assets/app-icon/`](../assets/app-icon):

- **macOS:** the Dock icon is set at startup, so it shows even with `cargo run`. To build a
  `Kuana.app` that uses `macos/Kuana.icns`, run `cargo install cargo-bundle` and then
  `cargo bundle --release`.
- **Windows:** `build.rs` embeds `windows/kuana.ico` into the executable, so it shows in
  Explorer, the title bar and the taskbar.
- **Linux:** the window's app id is `dev.kuana.editor`. Copy `linux/hicolor/` into
  `~/.local/share/icons/hicolor/` and `linux/dev.kuana.editor.desktop` into
  `~/.local/share/applications/`.

## Updates

Released builds check for updates shortly after launch and every few hours (unless
`auto_update` is `false`; **Check for Updates…** works either way), download in the background
and show **Restart to Update** in the status bar. They only install an update whose
`latest.json` carries a valid signature from the key in `src/updater.rs`. Builds from source
don't update themselves. **About Kuana** (first in the app menu, and in Find Action) shows the
version and the commit it was built from; **Copy Version** copies both for bug reports.

## First launch on macOS

Until the macOS app is signed with a Developer ID, it's only ad-hoc signed, so macOS
blocks the first launch of a copy downloaded with a browser: allow it under System Settings →
Privacy & Security → **Open Anyway**, or run `xattr -dr com.apple.quarantine /Applications/Kuana.app`.
The [install script](#install-script-macos-linux) and updates don't need this.
