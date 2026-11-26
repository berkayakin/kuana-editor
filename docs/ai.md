# AI

Kuana's AI features are opt-in: nothing is sent anywhere until you choose a model provider or
start an agent.

- [Agent sessions](#agent-sessions): coding agents (Kuana's own, Claude, Codex, Gemini CLI, GitHub
  Copilot and more) work in your project in a tab beside the code. You watch, allow what they ask
  and review what they changed.
- [Inline Assist](#inline-assist): select code, say how it should change, review the rewrite as a diff.
- [Tab completion](#tab-completion): code suggested as you type, from your own model.
- [Commit messages](#commit-messages), [conflicts](#resolving-conflicts) and
  [explanations](#asking-about-code) from the same model.
- [MCP servers](#mcp-servers) give agents more tools.
- Terminal agents connect to Kuana too: [Claude Code](editing.md#claude-code) and
  [Gemini CLI](editing.md#gemini-cli). GitHub Copilot's suggestions show as
  [ghost text](editing.md#edit-predictions).

## Choosing a model

**Settings → AI** (or **AI Settings…** in an agent menu) sets the model that Kuana Agent, Inline
Assist, commit messages and conflict resolution run on:

- **Provider**: Anthropic, OpenAI, OpenRouter, Google Gemini, Mistral, DeepSeek, xAI, Groq,
  Ollama or LM Studio (both on your machine, no key), or a custom OpenAI-compatible server.
- **API key**: kept in your system's keychain (macOS Keychain, Windows Credential Manager, the
  Secret Service on Linux), never in `settings.json`. Without one, Kuana uses the provider's usual
  environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`…) if it's set.
- **Model**: listed live from the provider once the key works, or typed by its id.
- **URL**: another address for the provider's API (a proxy, Ollama on another machine).
- **Effort**: how hard the model thinks, for the models that let Kuana say. More effort is slower
  and costs more.

In `settings.json`, these are the `ai` object (see [Settings](settings.md)). Only your own settings
can set it, not a project's.

## Agent sessions

The **Agents** view (the ✦ tab on the activity bar, **⌥⌘A** / **Alt+Shift+Y**) lists the
project's sessions: those waiting for your answer first, then the newest. A light shows what each
is doing: working, waiting for you, failed, open or saved. The field at the top filters them by
title or agent. **New Session** (+, **⌥⌘N** / **Alt+Shift+N**) picks an agent:

- **Kuana Agent**, built in, on the model you chose in Settings → AI. See [below](#kuana-agent).
- **Agents from the ACP registry**: Claude Agent (Claude Code's agent), Codex, Gemini CLI, GitHub
  Copilot, Qwen Code, OpenCode, Goose and others. They speak the
  [Agent Client Protocol](https://agentclientprotocol.com), so they work in Kuana as they do in Zed.
  Settings → AI → Agents installs, updates and uninstalls them. They go into Kuana's own folder:
  npm packages with your Node.js (or Kuana's own copy), binaries checked against the registry's
  checksum, and Python ones through `uv`. **Install an Agent…** in the New Session menu lists
  the rest, with a filter; one you pick installs, then its session starts. Agents that need you
  to sign in offer it in the session: a terminal method runs in a Kuana terminal tab.

A new session starts its agent at once, so its modes, models and `/` commands are there while you
write the first message. New Session (the shortcut) starts one with the agent you used last.

A session opens in a tab beside your code, which you can split, move and keep open like any tab.
Your messages sit on the right, the agent's answers across the column, as in Claude's apps, at
their own text size (15 points unless Settings → AI → Sessions → **Text size** says otherwise):

- **Reading and copying**: select any text in the transcript with the mouse (across messages too;
  a double click takes a word, a triple click a paragraph) and copy it with **⌘C** / **Ctrl+C**.
  Hovering a message shows **Copy**; yours also has **Edit** and, once the agent changed files,
  **Restore Files to Here**. A code block shows **Copy**, and **Run** for a shell command, which
  runs it in a Kuana terminal. A file the answer names (`src/main.rs:12`) opens at that line when
  clicked. When you've scrolled up, ↓ takes you back to the newest.
- **Going back** (Kuana Agent): **Retry** under the latest answer asks for it again, and **Edit**
  on an earlier message sends your changed text in its place: the conversation goes back to
  before it, for the agent too. If the agent changed files since, Kuana asks whether to put them
  back. Other agents can't go back (ACP has no way to), so there Edit puts the text in the
  composer to send as a new message.

- **The transcript**: your messages, the agent's answers in Markdown, its thinking (folded; Claude
  Agent sends Claude's thinking summaries, as Claude's apps show them, unless your Claude Code
  settings set `showThinkingSummaries`), and
  each step it takes as a card: files read, searches, edits with their diff (**Open Diff** shows
  it in full), commands with what they're for and their output (its end first; click for the
  rest). Runs of finished steps fold into one line ("Read 4 files, edited 2"). The agent's plan
  shows as a checklist, and a plan it asks you to approve shows whole, in Markdown.
- **Permissions**: when the agent asks before doing something, the card offers the agent's choices
  (Allow, Always Allow, Reject…). A session that waits for you lights its tab and its row, shows
  the question even if you'd scrolled up, and a toast tells you if you're elsewhere.
- **Notifications**: when a session finishes or needs your answer while you're in another window
  or app, the system tells you (as Claude's desktop app does) and the Dock icon bounces; clicking
  the notification brings the session back. macOS asks once whether Kuana may notify, when you
  start your first session. On Linux the desktop's notifications show it (a click isn't heard);
  on Windows the taskbar button flashes. Settings → AI → Sessions → **Notify me** turns it off.
- **Staying awake**: while a session works, the computer doesn't fall asleep from being idle (the
  display may still turn off), so a long task isn't cut short. Settings → AI → Sessions → **Keep
  the computer awake** turns it off.
- **The header**: the session's title and agent, how full its context is, and **⋯** for Copy as
  Markdown, Review Changes and Delete.
- **The composer**: **Enter** sends, **Shift+Enter** starts a new line, **Esc** stops the agent.
  Messages you send while it works wait their turn. Under the text, as in Claude's apps: the
  permission mode (Claude Agent's Manual, Accept edits, Plan, Auto…; Kuana Agent's Ask, Edit,
  Plan) and, by the send button, the model with its effort and switches (Fast mode…), each a menu
  that says what its choices do. Type **@** and part of a name to attach a project file, or **/**
  for the agent's commands: ↑/↓ pick, Tab or Enter takes the pick, Esc hides the list. Deleting
  an `@` mention before sending drops its file. The composer grows with what you write, up to a
  dozen lines.
  **↑** in an empty composer brings back your earlier prompts in this project (newest first, from
  any of its sessions); **↓** goes forward again and past the newest back to empty. Text you've
  typed is never replaced: ↑ then just moves the caret.
  **+** attaches the current file, the selection, the open files, the problems, the uncommitted
  changes (Git diff) or a picture file. **⌘V** / **Ctrl+V** with a picture on the clipboard (a
  screenshot, a design) attaches it too: PNG, JPEG, GIF or WebP, up to 5 MB, for agents that take
  pictures (Kuana Agent does, on a model that sees images). **Drop** files on the session (from
  Finder or the file tree) to attach them: text files and pictures join the message; a folder, or
  a file that's neither, is named in the prompt (`@src/`) for the agent to look at.
- **Changes**: the cap on the composer's top edge counts the files the agent changed. **Review** opens each
  as a diff against what it was before the session touched it. **Keep All** keeps them, and
  **Reject All** puts every file back as it was. Every agent write is one undoable edit in its open
  document, and Local History keeps the version before it ("Before an agent's edit"). Agents that
  write files themselves (Claude Agent) are followed too: Kuana keeps each file as it was when the
  agent's edit comes up. What each session changed is kept with it, so Review, Reject All and
  Restore still work after Kuana restarts.
- **Where it works**: a session works in the project folder of the file you're editing (the
  window's first folder otherwise). Before you send anything, the folder pill under its composer
  names that folder and changes it: with several folders, pick another; in a Git repository, pick
  a **New Worktree**, a checkout of its own on a new branch (`kuana/agent-…`) from the last commit,
  kept in Kuana's own folder, so the agent's edits touch neither your files nor other sessions'. Its pill then shows
  the branch, with **Open Worktree in New Window** (to run or test it), **Commit Changes on Its
  Branch** (the session's title as the message), **Merge into** your branch (commits what's left,
  then merges with Kuana's own merge; conflicts show in the Commit view as for any merge) and
  **Remove Worktree** (asks first; the branch goes too when it holds nothing new). A worktree
  made for a session that never sends anything goes when its tab closes; deleting a session
  removes its worktree and keeps its branch.
- **Checkpoints**: hover one of your messages and click ↶ (**Restore Files to Here**) to put back
  every file the agent changed from that message on, as it was just before. Files it created go
  to the Trash. A file that wasn't open goes back byte for byte, whatever its encoding.

Writes go through Kuana: an agent writing a file that's open edits its document (your unsaved
edits included), and a file outside the project asks first (`..` and links are followed to see
where a path really goes). Commands run in Kuana's terminals and
their output shows in the session.

Sessions are kept: each one's transcript is saved as it goes and when Kuana quits. Reopening a
project brings its session tabs back. Clicking a saved session in the Agents view opens it again.
An agent that can (Kuana Agent, Claude, others) continues the same conversation; one that can't
starts afresh, and the session says so.

Closing a project or its window stops its sessions (they're saved). Agents work in local projects;
in a [remote project](remote.md), run them in Kuana's terminal there.

### Kuana Agent

Kuana's own agent runs in Kuana on the model you chose. It lists and searches the project's files
(honoring `.gitignore`), reads them, edits them by replacing exact text, writes new ones, runs
commands, and reads the problems Kuana shows (language servers', linters'), something agents in a
terminal can't see. It follows the project's instructions in `AGENTS.md`, `CLAUDE.md` or
`.kuana/agent.md`.

Files an `.aiignore` or `.cursorignore` names (gitignore syntax, in the file's folder or any folder
above it) are kept away from it: it doesn't list, search, read or change them. Tab completion
doesn't send them either.

Its modes, under the composer:

| Mode | What it may do |
| --- | --- |
| **Ask** (the default) | Asks before each edit and each command |
| **Edit** | Edits files freely; asks before commands |
| **Plan** | Reads and plans; changes nothing |

**Always allow** covers edits, or one program (`cargo`, `npm`…), for the rest of the session. The
model menu switches the session's model. Its conversation with the model is kept in
`agents/kuana/` in Kuana's config folder, so it continues after a restart.

### Asking about code

From the editor's right-click menu or Find Action:

- **Ask Agent** (**⌥⌘I** / **Alt+Shift+K**): the selection (or the file) goes to the current
  session's composer, or a new session's.
- **Explain with Agent**: the selection, else the function or type around the caret, with a request
  to explain it.
- **Fix with Agent** (on a line with a problem): the problem and the lines around it, sent to fix.

## Dictation

Speak instead of typing, as in Claude's apps: click the **mic** in a session's composer (or
**⌥⌘V** / **Ctrl+Alt+V** there), talk, and click it again; or hold it down while you talk and let
go. Your words appear at the caret as you speak, underlined and dimmed until they're final, then
stay as typed text: send them, edit them, or keep dictating. **Esc** takes them back out, typing
meanwhile ends the dictation and keeps what was heard, and Undo takes a whole dictation back in one
step. Words after text you typed join it (a space, no capital mid-sentence). Your project's name,
its files' names and the files you attached are given to the recognizer as words to expect.
**Edit → Dictate** (Find Action: Dictate) does the same in any text field, such as a commit message.

- **macOS**: Apple's speech recognition, on the Mac where your language has an on-device model
  (most do; otherwise Apple's servers). macOS asks once for the microphone and for speech
  recognition.
- **Windows**: Windows' own speech recognition. It needs **Online speech recognition** on (Settings
  → Privacy & security → Speech) and the microphone allowed for desktop apps.
- **Linux**: there's no system speech recognition, so dictation records with PipeWire, PulseAudio or
  ALSA (`pw-record`, `parecord` or `arecord`) and sends the recording, when you stop, to a
  transcription service you choose: `"dictation": { "provider": "openai" }` in settings.json, with
  `"groq"`, or `"custom"` for a Whisper server of your own (its URL in Settings → AI). The words
  come once it answers.

Settings → AI → Dictation → **Language** sets the language you speak (the system's unless set).
`"dictation": { "engine": "service" }` sends audio to the service on macOS and Windows too.

## Inline Assist

Select code and press **⌃↩** (**Alt+Shift+I** on Windows and Linux), or choose **Inline Assist…** in
the editor's right-click menu, and say how it should change: "handle the error", "make it async",
"add docs". With nothing selected, Inline Assist writes new code at the caret.

The rewrite streams into a **Proposed by Kuana AI** tab beside the file, as a diff against the file.
**Accept** (**⌘↩** / **Ctrl+Enter**) puts it in the file as one edit you can undo (unsaved, so you can
look again); **Reject** (**⌘⌫** / **Ctrl+Backspace**), or closing the tab, drops it. While it's
being written, the same button stops it. If you edit the file before accepting, Kuana won't apply
the rewrite over your changes; run Inline Assist again.

In a SQL file with a [database connection](database.md), the model is told the database's kind
and the tables, views and columns the Database view has read, so "the ten customers who ordered
most this month" becomes SQL for your schema.

## Tab completion

Kuana can suggest code as you type, as dimmed ghost text, from the model you chose. Set **Settings →
AI → Edit predictions** to **Your AI model** (`"edit_prediction_provider": "ai"`), and pick a fast,
inexpensive **Tab completion model** there (`ai.completion_model`; the main model otherwise). A
moment after you stop typing, Kuana sends the code around the caret and shows the answer: **Tab**
accepts it, **Esc** or typing something else dismisses it, typing what it suggests keeps the rest,
and **⌥\\** (**Alt+\\**) asks right away. Each suggestion is one request to your provider, so it
costs what that request costs (nothing with Ollama or LM Studio). As with Copilot, files that hold
secrets (`.env`, keys, certificates) are never sent, nor files an `.aiignore` names;
`"edit_predictions": false` turns it off for a language or a project, and projects in Restricted Mode
don't get it.

## Commit messages

The ✦ button in the Commit view's message box (or **Write Commit Message with AI** in Find Action)
writes the message from what the commit would take: the staged changes, or every tracked change
when nothing is staged (with the unversioned files you checked). It follows the style of the
repository's recent commits. Notes you typed in the box first guide it ("fixes the login
timeout"). The message streams into the box; press the button again to stop.

## Resolving conflicts

In the [Merge view](git.md), **Resolve with AI** asks the model about each conflict that doesn't
combine by itself, with both sides, their common base and the lines around it, and puts the
answers in the result as one edit you can undo. Check the result, then **Apply**. Conflicts where
both sides changed different words are combined without the model, as **Resolve Simple** does.

## MCP servers

[MCP](https://modelcontextprotocol.io) servers give agents tools: a database, an issue tracker,
a browser. List them in `settings.json` under `ai`:

```jsonc
"ai": {
  "mcp_servers": {
    "github": { "command": "github-mcp-server", "args": ["stdio"], "env": { "GITHUB_TOKEN": "…" } }
  }
}
```

Every agent session gets them: agents from the registry use them in their own way, and Kuana Agent
starts them with its first prompt and offers their tools to the model. It asks before each tool call
(**Always allow** covers that tool for the session) and leaves them out in Plan mode. A server that
doesn't start is noted in the session's thinking. Settings → AI → Tools lists them.

## Shortcuts

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Agents view | ⌥⌘A | Alt+Shift+Y |
| New agent session | ⌥⌘N | Alt+Shift+N |
| Ask Agent | ⌥⌘I | Alt+Shift+K |
| Inline Assist | ⌃↩ | Alt+Shift+I |
| Accept / reject a proposed change (in its tab) | ⌘↩ / ⌘⌫ | Ctrl+Enter / Ctrl+Backspace |
| Send / new line / stop (in a session's composer) | ↩ / ⇧↩ / ⎋ | Enter / Shift+Enter / Esc |
| Pick an @ or / suggestion / take it / hide the list (in the composer) | ↑↓ / ⇥ / ⎋ | ↑↓ / Tab / Esc |
| Earlier prompts (in an empty composer) | ↑ / ↓ | ↑ / ↓ |
| Dictate / stop (in a session's composer) / take it back | ⌥⌘V / ⌥⌘V / ⎋ | Ctrl+Alt+V / Ctrl+Alt+V / Esc |

Explain with Agent, Fix with Agent and Write Commit Message with AI are in Find Action and the
menus.

## Privacy

Kuana sends your code only where you chose: to your model provider when you use Kuana Agent,
Inline Assist, commit messages or conflict resolution, and to the agents you start, which talk to
their own services. Keys stay in the keychain. Agents run on your machine as your user, so the
permissions they ask for matter: Kuana Agent asks before edits and commands unless you allow them.
In [Restricted Mode](settings.md#project-trust), Kuana Agent works as in any project; external
agents are your own programs and run as you start them.

Dictation listens only while the mic is lit. On macOS it's turned into text on the Mac where your
language allows, else by Apple; on Windows by Windows' online speech recognition; on Linux the
recording goes to the transcription service you set, and nowhere else.
