# Remote development

Kuana opens projects on other machines over SSH. The window, the open files and everything you
type stay on your computer, so typing never waits on the network; a small headless
`kuana-server` on the remote machine reads and writes its files, watches them, searches them and
runs Git there.

## Connecting

1. **File → Connect to SSH Host…** (or Find Action). The list has the projects you opened on other
   machines before, the hosts in your `~/.ssh/config` (and the files it `Include`s), and **Other
   Host…** for anything `ssh` takes (`user@host`, `user@host:2222` through your config).
2. Kuana connects with your own `ssh`, so your config, keys, agent and jump hosts work as in a
   terminal. A password, a key's passphrase or a new host's fingerprint is asked in Kuana.
3. The first time, Kuana copies `kuana-server` for that machine into `~/.kuana-server/<version>/`
   there (from its own copy when the machine has your system, otherwise from Kuana's release,
   checked against the release's signature). Later connections reuse it.
4. Kuana asks for a folder there (`~/project`, or a full path) and opens it as a project.

The window's project chip and Recent Projects show it as `host: ~/project`. A remote project comes
back like a local one: Recent Projects, and the last project at startup, connect first. One
connection serves every window on that host.

## What works there

- The file tree, opening, editing and saving files (an unsaved edit stays in Kuana until it's
  saved there), and file operations: new, rename, move, duplicate, delete (to that machine's
  trash, which Undo restores from).
- Changes made there, by a checkout or a build, show up as they do locally (the server watches the
  folder).
- Go to File, Find in Files and the TODO list, with the project's `.gitignore` applied there.
- Git: the gutter, blame, the Commit view, staging, commits, branches, pull and push, the log.
- Project settings (`.kuana/settings.json`), `.editorconfig`, formatters and ESLint run there.

Language servers, debuggers, terminals and Run tabs on the remote machine come next (see the
roadmap); until then they don't start for a remote project.

## When the connection drops

Kuana notices within a few seconds and offers **Reconnect**. The window and its unsaved text stay
as they were; once back, the tree, Git, the watcher and open files are read again.

## Trust

A remote folder is trusted (or not) on its own, by its host and path, as local folders are: until
you trust it, its project settings, formatters and Git hooks don't run.

## Notes

- The remote machine needs Linux (x86_64 or aarch64) or macOS. Windows machines aren't supported as
  remotes yet (WSL and containers are planned).
- Kuana keeps one SSH control connection per host (in its config folder, `ssh/`), as `ssh -o
  ControlMaster` does, for the commands that follow.
- `KUANA_SSH` names another `ssh` program; `KUANA_SERVER_BINARY` a `kuana-server` to copy instead of
  downloading one (development builds).
