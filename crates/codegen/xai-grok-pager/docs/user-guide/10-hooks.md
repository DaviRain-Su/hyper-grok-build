# Hooks

Hooks let you run a script or send an HTTP request at key moments in a Grok session. Use them to automate tasks, enforce safety checks, log activity, send notifications, and integrate your own tools.

---

## What Are Hooks?

A hook is a shell command or HTTP endpoint that Grok calls when a specific lifecycle event occurs. Hooks can:

- **Block actions** -- A `PreToolUse` hook can deny a dangerous command before it runs.
- **Keep the agent working** -- A `Stop` hook can block the agent from finishing its turn until a condition holds (e.g. the test suite passes) and feed the reason back to the model.
- **React to events** -- A `PostToolUse` hook can log every tool execution to a file.
- **Set up context** -- A `SessionStart` hook can export environment variables or run setup scripts.

---

## Common Use Cases

- **Safety guards**: Block commands such as `rm -rf /` before they run.
- **Audit logging**: Record tool use and sessions to a file or external service.
- **Notifications**: Send a message when a task finishes.
- **Auto-formatting**: Run `cargo fmt` or `prettier` after edits.
- **Environment setup**: Export variables at session start.
- **Custom workflows**: Trigger builds, tests, or deployments on specific events.

---

## Quick Start

1. Create the hooks directory:

   ```sh
   mkdir -p ~/.grok/hooks
   ```

2. Create a hook file, e.g. `~/.grok/hooks/session-start.json`:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "hooks": [
             { "type": "command", "command": "echo 'Grok session started in '$(pwd)" }
           ]
         }
       ]
     }
   }
   ```

3. Start (or restart) a Grok session. The hook runs automatically on `SessionStart`.

4. Press `Ctrl+L` on non–VS Code family terminals (or run `/hooks` anywhere — preferred on VS Code family) and check the Hooks tab to confirm it loaded.

---

## Hook Locations

Hooks are discovered from several places (all are merged):

| Scope | Path | Trusted? | Notes |
|-------|------|----------|-------|
| Global | `~/.grok/hooks/*.json` | Always | Personal hooks |
| Global | `~/.claude/settings.json` (and `settings.local.json`) | Always | Claude Code compatibility (configurable) |
| Global | `~/.cursor/hooks.json` | Always | Cursor compatibility (configurable) |
| Project | `<project>/.grok/hooks/*.json` | Requires trust | Per-repo automation |
| Project | `<project>/.claude/settings.json` (and `settings.local.json`) | Requires trust | Claude compatibility (configurable) |
| Project | `<project>/.cursor/hooks.json` | Requires trust | Cursor compatibility (configurable) |
| Plugin | Bundled inside installed plugins | Per-plugin | Shared team hooks |

The Claude and Cursor hook sources are scanned by default. To disable scanning for a specific vendor, set `[compat.<vendor>] hooks = false` in `~/.grok/config.toml` or the corresponding environment variable. See [Configuration](05-configuration.md#harness-compatibility) for details.

**Trusting a project**: The first time you open a project with hooks, you must trust it before its project hooks will run -- until then they are silently skipped. Grant trust by running `/hooks-trust` (or launching with `--trust`); the decision is recorded in the unified folder-trust store (`~/.grok/trusted_folders.toml`), the same gate that governs repo-local MCP/LSP servers. Global hooks in `~/.grok/hooks/` are always trusted and need no entry. This prevents untrusted repos from running arbitrary code.

Because hooks are unified under folder-trust, a `--trust` / `/hooks-trust` grant trusts the whole folder for **MCP, LSP, and hooks** together, and cascades to subdirectories. Conversely, disabling folder-trust (`GROK_FOLDER_TRUST=0` or `[folder_trust] enabled = false`) ungates project hooks along with MCP/LSP.

---

## Hook Events

| Event | When it fires | Blocking? |
|-------|---------------|-----------|
| `SessionStart` | A session starts. | No |
| `UserPromptSubmit` | You submit a prompt. | No |
| `PreToolUse` | A tool is about to run. | Yes — can deny |
| `PostToolUse` | A tool completes successfully. | No |
| `PostToolUseFailure` | A tool fails. | No |
| `PermissionDenied` | The permission system denies a tool call. | No |
| `Stop` | An agent turn ends on a genuine completion (not on a user interrupt). | Yes — can block the stop |
| `StopFailure` | A turn ends because of an API error. | No |
| `Notification` | The agent sends a notification. | No |
| `SubagentStart` | A subagent starts. | No |
| `SubagentStop` | A subagent's turn ends (fires once, in the subagent, with stop decision control). | Yes — can block the stop |
| `PreCompact` | Conversation compaction is about to run. | No |
| `PostCompact` | Conversation compaction completes. | No |
| `SessionEnd` | The session ends. | No |

`SubagentEnd` is accepted as an alias for `SubagentStop`. `PreToolUse` can block a tool call, and `Stop`/`SubagentStop` can block the agent from stopping (see [Stop Decision Control](#stop-decision-control)); every other event is passive.

### Cursor Hook Compatibility

Grok accepts Cursor's camelCase hook event names, so `~/.cursor/hooks.json` loads unchanged:

| Cursor event | Maps to |
|---|---|
| `sessionStart`, `sessionEnd` | `SessionStart`, `SessionEnd` |
| `preToolUse`, `postToolUse`, `postToolUseFailure` | `PreToolUse`, `PostToolUse`, `PostToolUseFailure` |
| `beforeShellExecution`, `beforeMCPExecution`, `beforeReadFile` | `PreToolUse` |
| `afterShellExecution`, `afterMCPExecution`, `afterFileEdit` | `PostToolUse` |
| `afterAgentResponse`, `afterAgentThought` | `PostToolUse` |
| `beforeSubmitPrompt` | `UserPromptSubmit` |
| `subagentStart`, `subagentStop` | `SubagentStart`, `SubagentStop` |
| `preCompact`, `stop` | `PreCompact`, `Stop` |

Cursor's per-operation hooks (`beforeShellExecution`, `afterFileEdit`, etc.) map to the generic `PreToolUse`/`PostToolUse` events. The hook script receives the tool name in the JSON input and can filter accordingly, or use the `matcher` field.

---

## The Hook JSON Format

Each `.json` file can define hooks for multiple events:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safety-check.sh", "timeout": 10 }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "bin/log-activity.sh" }
        ]
      }
    ]
  }
}
```

### Key Fields

- **Event name** (top-level key): any event listed in [Hook Events](#hook-events). Grok skips unrecognized event names so a shared Claude or Cursor settings file still loads.
- **matcher** (optional): A regular expression that selects which invocations trigger the hook. What it tests depends on the event: the tool name on tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`), the notification type on `Notification`, the subagent type on `SubagentStart`/`SubagentStop` (e.g. `explore`), the start source on `SessionStart` (`startup`, `resume`, …), the end reason on `SessionEnd`, the compaction trigger on `PreCompact`/`PostCompact` (`manual` or `auto`), and the error type on `StopFailure` (`rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, or `unknown`). A matcher on `Stop` or `UserPromptSubmit` is ignored with a warning (those events always fire). An empty or omitted matcher matches everything. The matcher tests the real tool name; MCP calls routed through the internal `use_tool` dispatcher appear as the qualified `server__tool` name (e.g. `linear__save_issue`), so match on that, not the dispatcher name.
- **type**: `"command"` (run a script or shell one-liner) or `"http"` (POST the event to a URL).
- **command**: Path to executable (relative to the JSON file) or inline shell command.
- **timeout**: Seconds before killing the hook (default: 5, or 600 for `Stop`/`SubagentStop` gates, matching Claude Code). All hook failures (timeouts, crashes, malformed output, missing required env vars) are fail-open: the failure is recorded for the UI scrollback but the tool call is not blocked. Only an explicit `deny` decision returned by the hook blocks a tool call.

### Tool Name Aliases

In a `matcher`, Grok maps Claude-style tool names to its own so hooks migrated from Claude fire correctly. Common aliases include:

- `Bash` → `run_terminal_command`
- `Read` → `read_file`
- `Edit`, `Write`, and `MultiEdit` → `search_replace`
- `Grep` → `grep`
- `Glob` and `ListDir` → `list_dir`
- `WebSearch` → `web_search`
- `Task` → `spawn_subagent`

A matcher keeps its original name too, so `Bash` matches both `Bash` and `run_terminal_command`.

---

## Writing Hook Scripts

### Input

The event is sent as JSON on **stdin** (for example, a `PreToolUse` event; the payload also always includes `toolUseId` and `toolInputTruncated`):

```json
{
  "hookEventName": "pre_tool_use",
  "sessionId": "abc-123",
  "cwd": "/Users/you/project",
  "workspaceRoot": "/Users/you/project",
  "permissionMode": "default",
  "toolName": "run_terminal_command",
  "toolInput": { "command": "npm test" },
  "timestamp": "2026-04-14T12:00:00Z"
}
```

Every event carries the same common fields: `hookEventName`, `sessionId`, `cwd`, `workspaceRoot`, `timestamp`, and `permissionMode` (`default`, `auto`, `plan`, or `bypassPermissions`), plus event-specific fields like `toolName` above.

### Output (Blocking Hooks)

For `PreToolUse` hooks, write JSON to **stdout**:

- **Allow**: `{"decision": "allow"}`
- **Deny**: `{"decision": "deny", "reason": "Unsafe command detected"}`

### Exit Codes

| Exit Code | Meaning |
|-----------|---------|
| `0` | Success / allow (for blocking hooks) |
| `2` | Explicit deny (`PreToolUse`) or block-stop with stderr as feedback (`Stop`/`SubagentStop`) |
| Other | Fail-open — the failure is recorded but nothing is blocked. For `PreToolUse`, a `deny` decision in stdout JSON is honored regardless of exit code. For `Stop`/`SubagentStop`, a valid decision JSON on stdout wins over the exit code (matching Claude Code); the exit code decides only when stdout has no usable JSON, in which case exit 2 blocks with stderr as the feedback. |

### Stop Decision Control

`Stop` and `SubagentStop` hooks run when the agent is about to finish its turn and can keep it working (Claude Code-compatible). Write JSON to **stdout**:

- **Block the stop**: `{"decision": "block", "reason": "The test suite hasn't been run yet"}`. The reason is fed back to the model as a user message and the agent runs another round in the same turn.
- **Non-error feedback**: `{"hookSpecificOutput": {"hookEventName": "Stop", "additionalContext": "Run the linter before finishing"}}`. Also keeps the agent working, but is surfaced as hook feedback rather than a hook error.
- **Force stop**: `{"continue": false, "stopReason": "Budget exhausted"}`. Ends the turn, overriding any blocks.
- **Allow the stop**: exit 0 with no output (or any non-JSON output).

Exiting with code `2` also blocks the stop, with **stderr** as the feedback.

The hook input includes `stopHookActive` and `lastAssistantMessage`. `stopHookActive` is true when the agent is already continuing due to a previous stop-hook block this turn; check it, or the transcript, to avoid blocking on a condition that will never resolve. `lastAssistantMessage` carries the text of the agent's final response this turn, so hooks can act on it without parsing the transcript. After **8 continuations** (blocks or non-error feedback) in one turn the gate is overridden and the turn ends; hooks are not consulted for that final, forced stop. The counter is per turn: the next user prompt starts fresh, so a long-running goal can span turns. Hook failures fail open: the agent stops normally.

`Stop` and `SubagentStop` hooks default to a 600-second timeout (matching Claude Code) because gates commonly run builds or test suites, and a timed-out hook fails open, so the agent stops anyway. Other events keep the 5-second default. Set `timeout` explicitly when a gate needs more: `{ "type": "command", "command": "bin/verify.sh", "timeout": 1200 }`.

The gate runs only for genuine completions. Interrupted (Esc / Ctrl+C), refused, and max-turns turns skip Stop hooks entirely, and API-error turns fire `StopFailure` instead. A separate Stop also fires at session end (`reason: "channel_closed"` or `"shutdown"`); its decision output is parsed but ignored, since there is no turn left to continue. A script that counts or gates on Stop fires should check `reason == "end_turn"` so the session-end fire doesn't skew it.

`StopFailure` is observation-only (use it to log failures or send alerts; output and exit code are ignored). Its input carries `error` (the classified type the matcher tests, in Claude Code's vocabulary: `rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, or `unknown` for anything the runtime cannot distinguish; capacity errors fold into `rate_limit` and there is no signal for `billing_error`), `errorDetails` (the raw error detail, when available), and `lastAssistantMessage` (the rendered error text shown in the conversation; for this event it is the error string, not assistant output).

`Stop` input also carries `backgroundTasks` and `sessionCrons`, so a hook can distinguish "session is done" from "session is paused waiting for background work to wake it back up". Both arrays are empty when nothing is in flight or scheduled. Each `backgroundTasks` entry describes one in-flight task: `id`, `type` (`shell`, `monitor`, or `subagent`), `status`, and (depending on the type) `command` (shell tasks only), `description` (a monitor's watched command line, or a subagent's task description), and `agentType` (subagents). Each `sessionCrons` entry describes one scheduled wakeup (`scheduler_create` or `/loop`): `id`, `schedule`, `recurring`, and `prompt`. The `schedule` value is a human-readable interval such as `every 5 minutes`; grok schedules are intervals, not cron expressions. Free-text entry fields are capped at 1000 characters with an in-string `… [+N chars]` marker.

Inside a subagent, the gate fires as `SubagentStop` (agent-frontmatter `Stop` hooks are automatically remapped). A `Stop` hook only gates the main agent.

`SubagentStop` fires once per subagent, at the subagent's own turn end, matching Claude Code. Its input carries a `phase` field (currently always `"gate"`) reserved for forward compatibility.

**Porting Claude Code stop hooks**: the output vocabulary (`decision`, `reason`, `continue`, `stopReason`, `additionalContext`) works unchanged. Check this list for what does not match Claude:

- **camelCase input**: grok's stdin envelope uses camelCase keys throughout where Claude uses snake_case. A script reading `.stop_hook_active`, `.hook_event_name`, or `.background_tasks[].agent_type` must switch to `.stopHookActive`, `.hookEventName`, and `.backgroundTasks[].agentType` (the event value is `"stop"`). Hooks registered through the grok-agent-sdk convert both the top-level keys and the `backgroundTasks`/`sessionCrons` entry keys to snake_case, so the wire's `.backgroundTasks[].agentType` reads as `.background_tasks[].agent_type` in the SDK.
- **`toolResult` field**: the `PostToolUse` tool output is `toolResult` (SDK: `tool_result`), not Claude's `tool_response`; a hook reading `.tool_response` must switch to `.toolResult`.
- **Session-end fire**: an extra observe-only Stop fires at session end; filter on `reason == "end_turn"` (see above).
- **Interval schedules**: `sessionCrons[].schedule` is a human-readable interval, never a cron expression.
- **Task types**: `backgroundTasks[].type` is only `shell`, `monitor`, or `subagent`; Claude's other labels (`workflow`, `teammate`, …) are not emitted.
- **StopFailure classes**: the emitted set is Claude Code's vocabulary — `rate_limit`, `authentication_failed`, `invalid_request`, `server_error`, `max_output_tokens`, `unknown`. grok emits a subset: capacity errors (503/529) fold into `rate_limit` as in Claude, and `billing_error` is never emitted (no signal), so a `billing_error` matcher will not fire.
- **permission_mode values**: grok emits `default`, `auto`, `plan`, or `bypassPermissions`. Claude's `acceptEdits`/`dontAsk` have no grok equivalent (grok's `auto` is the nearest), so a check like `permission_mode === "acceptEdits"` never matches.
- **Client (SDK) gate timeouts**: SDK `Stop`/`SubagentStop` gates default to 600 seconds like file hooks; `PreToolUse` client gates default to 30 seconds (the interactive hot path). Either can be overridden per matcher group via `timeoutS`, capped at 600.
- **`/goal`**: grok's goal loop is a separate feature that runs before the stop gate; it is not a prompt-type Stop hook.

A complete keep-working policy in one script:

```bash
#!/bin/bash
input=$(cat)
# Gate only genuine turn ends, not the session-end observe fire.
if [ "$(echo "$input" | jq -r '.reason')" != "end_turn" ]; then exit 0; fi
if ! bin/verify.sh >/dev/null 2>&1; then
  echo '{"decision": "block", "reason": "verify.sh failed; fix the failures before finishing"}'
fi
```

registered as `{ "type": "command", "command": "bin/stop-gate.sh", "timeout": 300 }` with `timeout` sized for the verify step. The hook fires again after each continuation, and the built-in cap ends the turn after 8; check `stopHookActive` to give up earlier on feedback the agent evidently cannot act on.

### Passive Hooks

For events like `SessionStart` or `PostToolUse`, stdout is ignored. Just exit 0 on success.

### Environment Variables

Grok sets several environment variables on every hook process. These are useful when writing context-aware or plugin-aware hook scripts.

#### Runner-injected variables (always available)

These variables are set by the hook runner for **every** hook:

| Variable              | Description |
|-----------------------|-------------|
| `GROK_HOOK_EVENT`     | The name of the event that triggered the hook (e.g. `pre_tool_use`, `session_start`, `post_tool_use`, `session_end`, `stop`, `notification`). |
| `GROK_HOOK_NAME`      | The configured name of this specific hook (includes the plugin prefix for plugin-provided hooks). |
| `GROK_SESSION_ID`     | The unique identifier of the current Grok session. |
| `GROK_WORKSPACE_ROOT` | Absolute path to the root of the current workspace. |
| `CLAUDE_PROJECT_DIR`  | Absolute path to the workspace root. A Claude Code-compatible alias for `GROK_WORKSPACE_ROOT`, set for every hook. |

These variables are **reserved**. Any values you attempt to set for them via the `env` field in your hook JSON are stripped at load time (a warning is logged), and the runner always injects the real values at spawn time.

#### Plugin hook variables

When a hook originates from a plugin, Grok additionally injects the following variables:

| Variable             | Description |
|----------------------|-------------|
| `GROK_PLUGIN_ROOT`   | Absolute path to the plugin's installed directory. |
| `GROK_PLUGIN_DATA`   | Absolute path to the plugin's writable data directory (for storing plugin state, caches, etc.). |

These values are provided by the plugin system. For the four plugin-related keys (`GROK_PLUGIN_ROOT`, `GROK_PLUGIN_DATA`, and their Claude aliases), the plugin adapter ensures the official plugin values always win over any user-declared values in the hook's `env` map.

#### User-defined environment variables

You can supply additional environment variables for an individual hook handler using the `env` field:

```json
{
  "type": "command",
  "command": "bin/my-hook.sh",
  "env": {
    "MY_SECRET": "value",
    "LOG_LEVEL": "debug"
  }
}
```

These variables are passed through to the hook process, but they cannot override the reserved runner or plugin variables listed above.

#### Using variables in `command` and `url` fields

Both `command` and `url` support `${VAR}` and `$VAR` expansion. See the custom-hooks reference for full details on load-time vs runtime expansion, the `env` map lookup order, and how parameter-expansion modifiers (e.g. `${VAR:-default}`) are handled.

---

## HTTP Hooks

Instead of a local script, call a remote endpoint:

```json
{ "type": "http", "url": "https://hooks.example.com/grok-event", "timeout": 15 }
```

The full event envelope is POSTed as JSON.

---

## Managing Hooks in the TUI

### The Hooks Tab

Press `Ctrl+L` on non–VS Code family terminals to open the Extensions modal (Plugins tab), or run `/hooks` (any terminal; required on VS Code family where `Ctrl+L` is interject) to open it on the Hooks tab. In the **Hooks** tab:

| Key | Action |
|-----|--------|
| `r` | Reload all hooks from disk |
| `a` | Add a custom hook by path |
| `x` | Remove the selected hook source (asks for confirmation; press lowercase `y` to confirm) |
| `Space` | Enable or disable the selected hook |
| `f` | Cycle the status filter (All / Enabled / Disabled) |

Hooks are grouped by source: **Global**, **Project**, **Plugin**, and **Custom**.

Each hook shows:
- **Event** it triggers on
- **Command** or **URL** that runs
- **Timeout** duration
- **Status** -- enabled or `[disabled]`

### Slash Commands

```
/hooks-list           # Show hooks loaded in this session
/hooks-trust          # Trust this project for hook execution
/hooks-add <path>     # Add a custom hook file or directory
/hooks-remove <path>  # Remove a custom hook
/hooks-untrust        # Revoke trust for this project
```

In the TUI pager, the individual `/hooks-*` commands do not appear in the slash-command list. The `/hooks` modal covers listing, adding, removing, and enabling or disabling hooks; project trust is managed via `/hooks-trust` (or the modal's Trust action), which writes the unified folder-trust store described above.

### Per-Hook Enable/Disable

Enable or disable an individual hook at runtime by pressing `Space` in the Hooks tab. The change takes effect immediately, without restarting the session.

### Mid-Session Reload

Press `r` in the Hooks tab to reload all hooks from disk. Grok re-reads every hook source, so this picks up changes you made to hook files during the session.

---

## Hook Annotations in Scrollback

When hooks execute, their results appear as annotations in the TUI scrollback. You can see which hooks ran, whether they allowed or denied an action, and any output they produced. These annotations appear only when the plugins UI is enabled (the default).

---

## Example: Safe Shell Guard

Block dangerous shell commands:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "bin/safe-shell.sh", "timeout": 5 }
        ]
      }
    ]
  }
}
```

Where `bin/safe-shell.sh`:

```bash
#!/bin/sh
INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

# Block destructive patterns
if echo "$CMD" | grep -qE '(rm -rf /|mkfs|dd if=|:(){ :|& };:)'; then
  echo '{"decision": "deny", "reason": "Blocked potentially destructive command"}' 
  exit 2
fi

echo '{"decision": "allow"}'
```

---

## Security Notes

- Global hooks (`~/.grok/hooks/`) run with your user permissions -- treat them like shell scripts.
- Project hooks require folder trust (`/hooks-trust` or `--trust`, the same gate as repo-local MCP/LSP) to prevent supply-chain attacks from malicious repos.
- HTTP hooks send session data -- only use trusted endpoints.

---

## Best Practices

1. **Keep hooks fast** -- long-running hooks block the UI. Use background processes (`&`) or async where possible.
2. **Use explicit `deny` to block** -- hooks fail-open on any error, so a hook that crashes will not block the tool. To enforce policy, your hook must run to completion and emit `{"decision":"deny","reason":"..."}` on stdout. Always handle errors inside your script so it can return an explicit decision.
3. **Use absolute paths or relative to hook file** -- scripts in `bin/` next to the JSON file are portable.
4. **Test with the modal** -- press `Ctrl+L` (non–VS Code family) or run `/hooks` to verify hooks are loaded and matching before relying on them.
5. **Version control project hooks** -- commit `.grok/hooks/` (but never secrets).

---

## Troubleshooting

- **Hook not running?** Press `Ctrl+L` on non–VS Code family (or run `/hooks` anywhere) to see if it is loaded and matched.
- **Project hooks ignored?** The folder may be untrusted. Run `/hooks-trust` (or relaunch with `--trust`).
- **Script not found?** Check the path is relative to the `.json` file and executable (`chmod +x`).
- **See errors?** Capture logs by launching with `RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok`, then check `/tmp/grok.log`.
