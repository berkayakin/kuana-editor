# AI

Kuana's AI features are opt-in: nothing is sent anywhere until you choose a model provider or
start an agent.

- [Agent sessions](#agent-sessions): coding agents (Kuana's own, Claude, Codex, Gemini CLI, GitHub
  Copilot and more) work in your project in a tab beside the code. You watch, allow what they ask
  and review what they changed.
- [Inline Assist](#inline-assist): select code, say how it should change, review the rewrite as a diff.
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
project's sessions, newest first. A light shows what each is doing: working, waiting for you,
failed, open or saved. **New Session** (+, **⌥⌘N** / **Alt+Shift+N**) picks an agent:

- **Kuana Agent**, built in, on the model you chose in Settings → AI. See [below](#kuana-agent).
- **Agents from the ACP registry**: Claude Agent (Claude Code's agent), Codex, Gemini CLI, GitHub
  Copilot, Qwen Code, OpenCode, Goose and others. They speak the
  [Agent Client Protocol](https://agentclientprotocol.com), so they work in Kuana as they do in Zed.
  Settings → AI → Agents installs, updates and uninstalls them. They go into Kuana's own folder:
  npm packages with your Node.js (or Kuana's own copy), binaries checked against the registry's
  checksum, and Python ones through `uv`. An agent you haven't installed is installed the first
  time you start a session with it. Agents that need you to sign in offer it in the session: a
  terminal method runs in a Kuana terminal tab.

A session opens in a tab beside your code, which you can split, move and keep open like any tab:

- **The transcript**: your messages, the agent's answers in Markdown, its thinking (folded), and
  each step it takes as a card: files read, searches, edits with their diff (**Open Diff** shows
  it in full), commands with their output as it runs. Runs of finished steps fold into one line
  ("Read 4 files, edited 2"). The agent's plan shows as a checklist.
- **Permissions**: when the agent asks before doing something, the card offers the agent's choices
  (Allow, Always Allow, Reject…). A session that waits for you lights its tab and its row, and a
  toast tells you if you're elsewhere.
- **The header**: the agent's modes and options (model, reasoning…) as menus, how full its
  context is, and **⋯** for Copy as Markdown, Review Changes and Delete.
- **The composer**: **Enter** sends, **Shift+Enter** starts a new line, **Esc** stops the agent.
  Messages you send while it works wait their turn. Type **@** and part of a name to attach a
  project file (Tab, Enter or a click picks the first match). Type **/** for the agent's commands.
  **+** attaches the current file, the selection, the open files, the problems or the uncommitted
  changes (Git diff).
- **Changes**: the bar over the composer counts the files the agent changed. **Review** opens each
  as a diff against what it was before the session touched it. **Keep All** keeps them, and
  **Reject All** puts every file back as it was. Every agent write is one undoable edit in its open
  document, and Local History keeps the version before it ("Before an agent's edit").

Writes go through Kuana: an agent writing a file that's open edits its document (your unsaved
edits included), and a file outside the project asks first. Commands run in Kuana's terminals and
their output shows in the session.

Sessions are kept: each one's transcript is saved as it goes and when Kuana quits. Reopening a
project brings its session tabs back. Clicking a saved session in the Agents view opens it again.
An agent that can (Kuana Agent, Claude, others) continues the same conversation; one that can't
starts afresh, and the session says so.

Agents work in local projects; in a [remote project](remote.md), run them in Kuana's terminal there.

### Kuana Agent

Kuana's own agent runs in Kuana on the model you chose. It lists and searches the project's files
(honoring `.gitignore`), reads them, edits them by replacing exact text, writes new ones, runs
commands, and reads the problems Kuana shows (language servers', linters'), something agents in a
terminal can't see. It follows the project's instructions in `AGENTS.md`, `CLAUDE.md` or
`.kuana/agent.md`.

Its modes, in the session's header:

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
| Attach the first @ suggestion (in the composer) | ⇥ | Tab |

Explain with Agent, Fix with Agent and Write Commit Message with AI are in Find Action and the
menus.

## Privacy

Kuana sends your code only where you chose: to your model provider when you use Kuana Agent,
Inline Assist, commit messages or conflict resolution, and to the agents you start, which talk to
their own services. Keys stay in the keychain. Agents run on your machine as your user, so the
permissions they ask for matter: Kuana Agent asks before edits and commands unless you allow them.
In [Restricted Mode](settings.md#project-trust), Kuana Agent works as in any project; external
agents are your own programs and run as you start them.
