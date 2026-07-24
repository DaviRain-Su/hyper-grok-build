# Session Management

Grok saves every conversation to disk automatically. Whether you work in the TUI, in headless mode, or over agent stdio, Grok records the exchange as a session. You can resume, rewind, or compact it. This document describes how to manage sessions.

---

## What Sessions Are

A session is a persistent conversation with full history. It includes:

- All user prompts and agent responses
- Tool calls and their results
- TODO/task list state
- File snapshots for rewind
- Token usage and turn counts
- Subagent sessions (when enabled)

Sessions are identified by a unique session ID (a UUIDv7 when Grok generates it; a client may supply its own ID with `-s`) and stored on disk under `~/.grok/sessions/`. Set `GROK_HOME` to override the base directory; when it is unset, Grok uses `~/.grok`.

---

## Storage Layout

Grok stores each session in its own directory, grouped by working directory. It URL-encodes the working directory to name the group. When the encoded name exceeds 255 bytes, it instead uses a slug plus a hash and records the original path in a `.cwd` file inside the group.

```
~/.grok/sessions/<encoded-cwd>/<session-id>/
  summary.json            # metadata: summary/title, timestamps, model ID, message counts
  updates.jsonl           # ACP session update stream (conversation + tool calls)
  chat_history.jsonl      # raw chat messages sent to the model
  plan.json               # TODO/task list state
  rewind_points.jsonl     # file snapshots for /rewind undo
  signals.json            # session signals (token usage, tool/turn counters)
  feedback.jsonl          # user feedback and ratings
  compaction_checkpoints/ # saved state from compaction (manual or auto)
  subagents/              # per-subagent metadata (meta.json); the child sessions live in the normal sessions tree
```

`summary.json` is the index entry. It records the session summary and generated title, the model ID, the creation and update timestamps, the message counts, and a parent session reference for forked or restored sessions. `updates.jsonl` is the authoritative conversation log that drives `/resume` and session restore.

---

## Starting and Ending Sessions

### New Session

The TUI creates a new session each time you launch. To explicitly start fresh mid-session:

```
/new
```

This clears the current context and begins a new conversation. Alias: `/clear`.

### Exit

End the session and quit Grok:

```
/quit
```

Alias: `/exit`. To leave the current session but stay in Grok, use `/home` to return to the welcome screen.

---

## Resuming Sessions

### From the TUI

Use the `/resume` command to browse and resume previous sessions:

```
/resume
```

This opens a session picker that lists recent sessions for the current workspace. Select a session to resume it. The command takes no arguments.

Typing in the picker filters the list by title and also searches your conversation content as you type; content matches appear under an "Extended search results" heading. Press `Ctrl+/` to search immediately without the brief pause.

To switch between, rename, or close the sessions that are currently active (the parent session and any forks), use `/dashboard` (or its alias `/sessions`) instead.

### From the Command Line

Resume a specific session by ID or title:

```bash
grok --resume <session-id-or-title>
```

A value that is not a session ID is matched against session titles for the current directory, ignoring letter case (a simple lowercase comparison) — handy after `/rename`. If several sessions share the title, a single manually renamed session wins over auto-generated duplicates; otherwise the command errors and lists the matching IDs. UUID-shaped values are always treated as session IDs, never titles. Scripts should prefer IDs.

Run `grok --resume` without a value to resume the most recent session for the current directory.

### From the Welcome Screen

When you launch `grok`, the welcome screen lists recent sessions for the current directory. Select one to resume it.

---

## Forking and Renaming Sessions

### Fork

Branch the current session into a peer agent that starts from a copy of the conversation:

```
/fork [--worktree|--no-worktree] [directive]
```

Pass an optional `directive` to set the new session's first prompt. Use `--worktree` or `--no-worktree` to choose whether the fork runs in a new git worktree; omit both to be asked each time. The `--at <turn>` flag is not supported in this version.

### Rename

Rename the current session's title:

```
/rename <title>
```

Alias: `/title`.

---

## The /rewind Command

`/rewind` undoes recent changes by restoring files to their state at an earlier point in the conversation. Use it to recover from mistakes.

```
/rewind
```

When you run `/rewind` (or press **Esc Esc** within 800ms while idle with an empty prompt and conversation messages), Grok:

1. Shows a list of rewind points (one per user prompt)
2. Lets you select which point to rewind to
3. Restores all files to their state at that point
4. Truncates the conversation history to that point

File snapshots are recorded at each prompt, so you can go back to any previous state.

**Important:** `/rewind` modifies files on disk. The changes it reverts are lost unless you have them in git.

---

## The /compact Command

`/compact` compresses the conversation history to save context window space. Use it in long sessions where early messages are no longer relevant.

```
/compact
/compact [context]
```

The optional `context` argument lets you provide additional instructions about what to preserve during compaction.

### Auto-Compact

Grok automatically compacts the conversation when the context window approaches its limit. You will see a notification when auto-compact triggers. The `context_window` setting on your model configuration controls when this threshold is reached.

---

## The /session-info Command

View details about the current session:

```
/session-info
```

This shows:

- Session title (when set)
- Shell version
- Auth method (OAuth vs API key) and where to manage account and credits (https://grok.com/?_s=billing for OAuth, console.x.ai for API key; API-key sessions also suggest `grok login` for SuperGrok)
- Session ID
- Working directory
- Model (with a model hash for coding models)
- API backend and sandbox profile (when set)
- Context window usage (used and total tokens, with the percentage used)

---

## Headless Session Management

In headless mode, you manage sessions through command-line flags:

```bash
# New session each time (default)
grok -p "Hello"

# Resume an existing session by ID or title (errors if it does not exist)
grok -p "Continue where we left off" -r <session-id-or-title>

# Continue the most recent session in the current directory
grok -p "What were we doing?" -c
```

In headless mode, resume an existing session with `-r`/`--resume`, which errors if the session does not exist, or continue the most recent session in the current directory with `-c`/`--continue`. A non-ID value is matched against session titles for the current directory, ignoring letter case (a sole manually renamed match wins among duplicates; remaining duplicates error with their IDs; UUID-shaped values always take the ID path) — scripts should pass the session ID from JSON output (see below) to `-r`.

Use `-s`/`--session-id` only to **create** a new session with a **UUID** (errors if the value is not a UUID, or if that ID already has a session under the target session directory). It does **not** resume an existing session — that was the old hidden upsert behavior; use `-r`/`-c` instead. Combine `-s` with `-r`/`-c` only when also passing `--fork-session` (forks history into a new ID; optional `-s` names the child UUID). This matches Claude Code’s anti-overwrite model (client preflight under the write cwd; sequential use is reliable, concurrent same-ID is best-effort).

To read the session ID back, request JSON output:

```bash
grok -p "Hello" --output-format json | jq -r '.sessionId'
```

---

## Agent stdio Session Management

When building with ACP, sessions are managed via protocol methods:

```typescript
// Create new session
const { sessionId } = await connection.request("session/new", {
  cwd: "/path/to/project",
  mcpServers: [],
});

// Load existing session
await connection.request("session/load", {
  sessionId: "existing-session-id",
  cwd: "/path/to/project",
  mcpServers: [],
});
```

The agent persists all session updates automatically. Clients can reconnect and load previous sessions by ID.

---

## The grok sessions Subcommand

List or search sessions from the command line. `grok sessions` requires a subcommand:

```bash
# List recent sessions for the current directory
grok sessions list

# Limit the number of results (default 20)
grok sessions list --limit 50

# Search sessions by keyword (matches titles and prompts)
grok sessions search "rate limit"
```

`grok sessions list` shows sessions for the current working directory, grouped by worktree label. Each row lists the session ID, the creation and update dates, the source status, and the summary. `grok sessions search` combines a local SQLite index with remote results.

---

## Worktree Sessions

When working with subagents or session forks, Grok can create isolated git worktrees per session. Each worktree gets its own copy of the working directory, so file changes in one session do not affect another.

Worktree sessions are managed internally through the `x.ai/git/worktree/*` extension methods. Key operations:

- **Create**: Create a new worktree for an isolated session
- **Apply**: Merge worktree changes back into the main working directory
- **Remove**: Clean up a worktree when the session is done

Resume a session in a fresh worktree with `grok -w -r <session-id>`.

---

## Session Storage Details

### Persistence Format

Grok stores the conversation as newline-delimited JSON (JSONL). Each line in `updates.jsonl` is a self-contained ACP session update event. This format supports:

- Incremental writes (append-only during a session)
- Efficient streaming reads (for session restore)
- Easy debugging (each line is valid JSON)

The smaller state files -- `summary.json`, `plan.json`, and `signals.json` -- are plain JSON rather than JSONL. JSONL is the source of truth for session content; `grok sessions search` additionally maintains a local SQLite FTS5 index over session titles and prompts for fast keyword search.

### Session Metadata

`summary.json` records, among other fields:

- `info` -- the session ID and working directory
- `session_summary` and `generated_title` -- the session summary and its model-generated title
- `created_at` and `updated_at` -- creation and last-update timestamps
- `num_messages` and `num_chat_messages` -- update and chat-message counts
- `current_model_id` -- the model in use
- `parent_session_id` -- the source session for a fork or restore
- `agent_name` -- the agent definition active when the session was last saved

### Disk Usage

Rewind point snapshots (copies of modified files) are the largest contributor to disk usage in sessions that modify many files. Use `/compact` to reduce history size.

---

## Tips

- Use `/new` to start fresh when your current context is no longer relevant.
- Use `/compact` proactively in long sessions to keep the context window effective.
- Use `/rewind` to undo mistakes; it restores actual file snapshots instead of relying on the agent to reconstruct earlier state.
- In headless mode, capture the `sessionId` from JSON output and pass it to `-r` to build multi-step automations that maintain context.
- Check `/session-info` to see how much of your context window has been used.
