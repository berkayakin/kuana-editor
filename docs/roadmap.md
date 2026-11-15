# Roadmap

Where Kuana is going, roughly in order. Nothing here has a date, and plans change with what
people run into. To push something up, open or upvote an
[issue](https://github.com/berkayakin/kuana-editor/issues); to work on something, say so in its
issue first so effort isn't doubled.

What's already there is in [Features](features.md).

## Now: dependable everywhere

Making what's there dependable on every platform before adding more.

- **Windows and Linux out of beta**: daily use on both, fixes for what it finds. Every release is
  already started and driven through its main areas on all three platforms by CI (`smoke.yml`).
- **Signed builds**: a Developer ID-signed and notarized macOS app, a signed Windows installer.
- **Linux packages** beside the `.tar.gz` (x64 and Arm64 builds exist for Windows and Linux): an
  AppImage that updates itself, then a package repository or Flatpak, since a `.deb` installed
  system-wide can't take Kuana's own updates.
- **Crash reports**: after a crash, Kuana offers to open a GitHub issue filled in from its log
  (done); fixing what comes in comes first.
- **More languages in the [registry](https://github.com/berkayakin/kuana-extensions)** as people
  ask for them (it has over 50, C/C++, Kotlin and Swift among them).

## Next

- **Editing**: spell checking in comments, strings and Markdown.
- **Git**: reordering commits and splitting one (interactive rebase beyond reword, squash and drop), the whole log
  instead of the latest 3,000 commits.
- **Debugger**: hit counts, exception breakpoint options, attaching to a debugpy or Node inspector
  port, debug adapters from extensions.
- **More of Vim** in the existing [Vim mode](editing.md#vim-mode): registers other than the
  clipboard, macros, marks, `:s`, `gu`/`gU`.
- **[Remote development](remote.md)**: installing a missing language server on the remote machine
  with one click, terminals that keep running through a short drop, Windows machines over SSH.

## Later, being considered

Bigger pieces that need a design first; feedback on which matter most is welcome.

- Screen reader support, which depends on the GPUI framework.
- Localization.
