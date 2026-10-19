# Git

Kuana uses the `git` command line, so your config, credential helpers, SSH keys and hooks
all apply. Prompts are disabled, so a remote that needs a password fails fast with a
message instead of hanging; use an SSH agent or a credential helper.

- **Gutter:** green/blue/red bars mark added, changed and deleted lines against `HEAD`,
  updated as you type; staged changes are drawn faded. Click a bar to see the lines it replaced
  inline, above the new ones (Escape hides them), and **Stage** (or **Unstage**) just that change,
  or **Rollback** it. Stage Lines / Unstage Lines are
  also in the editor's right-click menu and Find Action, and stage what the editor shows, unsaved
  edits included.
- **Inline blame:** the caret line shows its author, age and commit message
  (Find Action → Toggle Inline Git Blame).
- **File tree:** changed files are colored with a status letter; folders containing
  changes are tinted. What Git ignores (`.gitignore` files, `.git/info/exclude` and your
  global excludes) is dimmed, an ignored folder with everything in it.
- **Commit panel** (⌘K / ⌘0, or the Commit tab above the sidebar, whose tooltip shows the
  number of changes): the branch with fetch, pull and push at the top, then staged changes,
  unstaged changes and **Unversioned Files** (new files Git doesn't track), with stage, unstage,
  open and roll back on hover (what a rollback throws away is kept in
  [Local History](editing.md#local-history)); click a file for its diff, which opens in a tab of the current pane like the file itself. Write a message and press ⌘↩ to commit
  (with nothing staged, **Commit All** takes every change to tracked files), or use **Commit &
  Push**. Unversioned files go into a commit only when you check them, as in JetBrains: each has
  a checkbox, unchecked to start with, and the section's heading checks or clears them all.
  **Amend** starts from the previous message. In a folder that isn't a repository it offers
  **Initialize Repository**.
- **Diffs** open as tabs (split, move and close them like any other) and show side by side or
  unified (the choice is remembered), with syntax colors and the changed words within a line
  highlighted. A diff of unsaved edits follows them as you type; once a file has nothing left to
  stage, its diff tab closes. Each change has **Stage** / **Unstage**; F7 / ⇧F7 step
  through changes; collapsed unchanged lines expand on click. Images show the previous and new
  version side by side; other binary files say there's no text diff.
- **Branches:** the title bar shows the current branch next to the project name; the Commit
  panel's header shows it too, with the number of commits to pull and to push on its Pull and
  Push buttons. Click the branch (or use **Branches…** in the Git
  menu) to switch to a local or remote branch, or type a new name to create one. Hover another branch
  for **Merge** (into the current one) and **Rebase** (the current one onto it), and a local one
  for **Rename** and **Delete** (Kuana says when a branch has commits no other branch has, and
  deletes it anyway only if you confirm).
- **Stash:** **Stash Changes…** (the archive button on the Commit panel's first section, the Git
  menu, Find Action) stashes the changes to tracked files, with an optional message; check
  **Include untracked files** to stash those too (checked to start with only when they're all
  there is). Stashes are listed under the changes; hover one to **Pop** (apply and drop),
  **Apply** (keep it) or **Drop** it (after asking). A stash that conflicts leaves the files
  conflicted and the stash kept. When untracked files from a stash exist again (a build
  regenerated them), Git would apply the rest and keep the stash, so applying it again would
  repeat that: instead you're asked first, and can move the existing copies to the Trash and
  apply the whole stash. Anything that still applies only in part is reported as such.
- **Merge conflicts:** a merge or rebase that stops on conflicts shows a banner in the Commit
  panel with **Abort**. Conflicted files open in the editor, where each conflict is tinted
  (current side green, incoming blue) with **Accept Current / Accept Incoming / Accept Both**
  on its first line (also in the right-click menu and Find Action). Mark a file resolved with
  ✓ on hover (Kuana asks if markers remain), then **Commit Merge** / **Continue**. The merge
  message is filled in, and committing is blocked while conflicts remain.
- **Log** (⌘9 / Alt+9, or its button at the top right): commits of all branches (toggle) with a branch
  graph, branch and tag labels, and a filter by message, author, hash or branch. Select a commit
  for its message, author, date, hash (copy) and changed files; click a file for its diff.
  Right-click a commit to copy its hash, **Checkout Revision** (detached HEAD), create a **New
  Branch…** or **New Tag…** there, **Cherry-Pick** it onto the current branch, **Revert** it, or
  **Reset Here** soft (changes kept staged), mixed (kept unstaged) or hard (discarded, after
  asking). A cherry-pick or revert that conflicts stops like a merge, with Abort / Continue.
  Questions about something that can't be undone (a hard reset, rolling back, dropping a stash,
  deleting a branch with commits no other branch has) have **Cancel** as the default: Return
  doesn't confirm them.
  **Show History** (editor right-click menu, Git menu) lists one file's commits, following renames.
- **Push / Pull / Fetch:** from the Commit panel, the keyboard, or Find Action. Pushing a
  new branch sets its upstream. Results appear as notifications.
- **Update Project** (pull) fast-forwards when only the remote has new commits. When both your
  branch and its upstream have new commits, the **pull strategy** decides: **Merge** (a merge
  commit), **Rebase** (yours replayed on top of the incoming ones) or **Fast-forward only**
  (stop and say so). It's in Settings → Git (`pull_strategy`); unset, your Git config's
  `pull.rebase` / `pull.ff` decide, and without those the first pull that needs it asks Merge or
  Rebase and keeps the answer.
- **Remotes:** **Manage Remotes…** (Git menu, Find Action, or the cloud button in the Push
  dialog) lists each remote with its URL; add one, or hover one to **Edit** its name and URL or
  **Remove** it (after asking; the remote repository isn't touched). A new branch is pushed to
  `origin`, else the first remote; with several, the Push dialog lets you pick.
- **Push** (⌘⇧K / Ctrl+Shift+K) first opens a dialog with the branch and where it goes (a new
  branch is marked, with a choice of remote when there are several), and the commits to push. Click a commit for its message and files, and a
  file for its diff (closing that tab returns to the dialog). It warns when the remote has commits you
  don't have; **Force push** replaces them only if the remote hasn't moved since your last fetch
  (`--force-with-lease`). Enter pushes, Esc cancels.

- **Clone:** **Clone Repository…** on the welcome screen, in the File and Git menus, or Find Action
  (also as "Get from Version Control"). Enter an HTTPS or SSH URL (one on the clipboard is
  filled in) and a parent folder; progress is shown live, **Stop** cancels and removes the partial
  folder, and the clone opens when it's done. **Shallow clone** fetches only the latest commit.
- **Welcome screen:** with no tabs open, it shows the current branch, ahead/behind counts,
  changed files, the last commit and the remote (click to open it in the browser), with
  Commit, Update, Push, Fetch and Branches. In a folder that isn't a repository it offers
  **Initialize**.

Status follows changes on disk as they happen: a change in the working tree reads the status
of the files it touched (not the whole repository), and all of it is read again after Git
actions and when something else changes `.git` (a commit or checkout in a terminal). A project
folder that can't be watched is read every few seconds instead. Open files without unsaved
edits reload when a checkout, pull or rollback changes them on disk; carets, breakpoints, folds
and problems stay with the lines they were on, and Undo brings back the text from before.
