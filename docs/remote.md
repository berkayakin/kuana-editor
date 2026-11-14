# Remote development

Kuana opens projects on other machines over SSH, in WSL distributions and in containers. The
window, the open files and everything you type stay on your computer, so typing never waits on the
network; a small headless `kuana-server` on the remote machine reads and writes its files, watches
them, searches them and runs Git there.

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

Recent Projects show it as `host: ~/project`, and the status bar's first item names the machine. A remote project comes
back like a local one: Recent Projects, and the last project at startup, connect first. One
connection serves every window on that host.

## WSL (Windows)

**File → Connect to WSL…** lists your distributions (the default one first). Kuana runs its
server in the one you pick, as your default user there, and asks for a folder in it. Everything
below works as it does over SSH: files, Git, language servers, terminals, Run and debugging all
happen inside the distribution, with its own tools.

## Containers

- **File → Attach to Running Container…** lists `docker ps`; pick one and a folder in it.
- **File → Open in Dev Container** reopens the current project in its dev container
  (`.devcontainer/devcontainer.json`, `.devcontainer.json`, or `.devcontainer/<name>/devcontainer.json`).
  The first time, the container is made: from its `image` or its Dockerfile (`build`), with the
  project mounted at `workspaceFolder` (`/workspaces/<folder>` unless it says otherwise), its
  `containerEnv`, `mounts`, `runArgs` and `containerUser`, then its `onCreateCommand`,
  `updateContentCommand`, `postCreateCommand` and `postStartCommand`. Later it's found again (and
  started if it stopped). You work in it as its `remoteUser`.
  Opening a project that has a dev container offers it once.
- With the [devcontainer CLI](https://github.com/devcontainers/cli) installed
  (`npm install -g @devcontainers/cli`), Kuana lets it bring the container up instead, so
  features and Docker Compose work too; without it, those are left out (and said so).
- Kuana copies its server into the container (into the user's home, `~/.kuana-server/`). A stopped
  container that was opened before is started when its project is opened again.
- Docker's CLI is found on your shell's `PATH`; `KUANA_DOCKER` names another (`podman`).
- A dev container runs commands from the project, so it opens only for a trusted project.

## What works there

- The file tree, opening, editing and saving files (an unsaved edit stays in Kuana until it's
  saved there), and file operations: new, rename, move, duplicate, delete (to that machine's
  trash, which Undo restores from).
- Changes made there, by a checkout or a build, show up as they do locally (the server watches the
  folder).
- Go to File, Find in Files and the TODO list, with the project's `.gitignore` applied there.
- Git: the gutter, blame, the Commit view, staging, commits, branches, pull and push, the log.
- Project settings (`.kuana/settings.json`), `.editorconfig`, formatters and ESLint run there.
- Language servers run there, found on that machine's `PATH` as your login shell sets it (and in
  the project's `node_modules` or virtual environment), and told that machine's own paths.
- Terminals open a shell there (your login shell on that machine), with shell integration: command
  marks, Copy Command Output, the folder a link is relative to. Closing one asks first while a
  program runs in it, as locally.
- Run tabs and test runs run there, and their errors become Problems with links into the project.
- Debugging: the build and the debug adapter (lldb-dap, Delve, debugpy, js-debug) run there;
  adapters that listen on a port are reached through the connection, so nothing needs to be open
  on that machine's firewall. Attaching to a running process is local only for now.
- Git and SSH questions asked there (a password, a passphrase, a new host for `git pull`) are
  asked in Kuana.

## Ports

A web app or a database running there is reached from here through a forwarded port: **Forward a
Port…** (Find Action, or the status bar's remote item) asks for its port there and listens on the
same port here when it's free (another one when it isn't; the toast says which). The status bar's
item lists them, to open one in the browser or stop it. A project keeps its ports: they're
forwarded again when it opens or reconnects. A dev container's `forwardPorts` are forwarded when it
opens (ports of other Compose services, `db:5432`, aren't).

## When the connection drops

Kuana notices within a few seconds: the status bar's remote item turns red and Kuana offers
**Reconnect** (also in that item's menu). The window and its unsaved text stay as they were; once
back, the tree, Git, the watcher and open files are read again, language servers start again and
forwarded ports come back.

## Trust

A remote folder is trusted (or not) on its own, by its host and path, as local folders are: until
you trust it, its project settings, formatters and Git hooks don't run.

## Notes

- The remote machine needs Linux (x86_64 or aarch64) or macOS. Windows machines aren't supported as
  SSH remotes; use WSL there.
- Kuana keeps one SSH control connection per host (in its config folder, `ssh/`), as `ssh -o
  ControlMaster` does, for the commands that follow.
- `KUANA_SSH`, `KUANA_WSL` and `KUANA_DOCKER` name other programs for each; `KUANA_SERVER_BINARY` a
  `kuana-server` to copy instead of downloading one (development builds).
