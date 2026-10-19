# Roadmap

Where Kuana is going, roughly in order. Nothing here has a date, and plans change with what
people run into. To push something up, open or upvote an
[issue](https://github.com/berkayakin/kuana-editor/issues); to work on something, say so in its
issue first so effort isn't doubled.

What's already there is in [Features](features.md).

## Now: a solid 0.2

Making what's there dependable on every platform before adding more.

- **Windows and Linux out of beta**: daily use on both, fixes for what it finds. Every release is
  already started and driven through its main areas on all three platforms by CI (`smoke.yml`).
- **Signed builds**: a Developer ID-signed and notarized macOS app, a signed Windows installer.
- **Linux packages**: AppImage and `.deb` beside the `.tar.gz`; ARM64 builds for Linux and Windows.
- **Crash reports**: after a crash, Kuana offers to open a GitHub issue filled in from its log
  (done); fixing what comes in comes first.
- **More languages in the [registry](https://github.com/berkayakin/kuana-extensions)**: C/C++
  (clangd) first, then Kotlin and Swift.

## Next

- **Editing**: semantic highlighting from language servers, Emmet, spell checking in comments,
  strings and Markdown, ESLint as you type (through its language server).
- **Git**: a three-way merge editor, interactive rebase, deleting remote branches, the whole log
  instead of the latest 3,000 commits.
- **Debugger**: hit counts, exception breakpoint options, attaching to a debugpy or Node inspector
  port, debug adapters from extensions.
- **More of Vim** in the existing [Vim mode](editing.md#vim-mode): registers other than the
  clipboard, macros, marks, `:s`, `gu`/`gU`.

## Later, being considered

Bigger pieces that need a design first; feedback on which matter most is welcome.

- Remote development: projects over SSH, and WSL folders on Windows.
- Several folders in one window (multi-root workspaces).
- Screen reader support, which depends on the GPUI framework.
- Localization.
