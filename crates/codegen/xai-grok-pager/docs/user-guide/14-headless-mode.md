# Headless Mode and Scripting

Headless mode runs Grok non-interactively from the command line. It accepts a single prompt, executes it with full tool access, and returns the result. Use it to automate tasks, script workflows, build integrations, and parse output programmatically.

---

## Basic Usage

Passing a prompt non-interactively triggers headless mode. The most common way is the `-p` flag (short for `--single`); `--prompt-json` and `--prompt-file` also trigger it:

```bash
grok -p "Your prompt here"
```

Grok processes the prompt, runs any necessary tools, and prints the result to stdout. The process exits when the response is complete.

---

## Command-Line Options

| Flag                    | Description                                           |
| ----------------------- | ----------------------------------------------------- |
| `-p, --single <PROMPT>` | The prompt to send (or use `--prompt-json` / `--prompt-file`) |
| `-m, --model <MODEL>`   | Model to use (e.g., `grok-build`)              |
| `-s, --session-id <ID>` | Create a **new** session with this **UUID** (errors if invalid UUID or already in use under the target session directory; does not resume — use `-r`/`-c`) |
| `--fork-session`        | With `-r`/`-c`, fork into a new session ID instead of appending to the original |
| `-r, --resume <ID_OR_TITLE>` | Resume an existing session by ID, or by title for the current directory, ignoring letter case (a sole manually renamed match wins among duplicates; remaining duplicates error with their IDs; UUID-shaped values always take the ID path; scripts should prefer IDs) |
| `-c, --continue`        | Continue the most recent session in current directory  |
| `--cwd <PATH>`          | Set working directory                                 |
| `--output-format <FMT>` | Output format: `plain`, `json`, `streaming-json`      |
| `--yolo`                | Auto-approve all tool executions                      |
| `--rules <TEXT>`        | Custom rules for the system prompt                    |
| `--tools <TOOLS>`       | Allowlist of built-in tools (comma-separated). MCP meta-tools remain available unless denied. Headless only. |
| `--disallowed-tools <TOOLS>` | Denylist of built-in tools to remove (comma-separated). Supports `Agent` entries. Headless only. |
| `--max-turns <N>`       | Maximum number of agentic turns before stopping. Headless only. |
| `--reasoning-effort` / `--effort <LEVEL>` | Reasoning effort for reasoning models. Canonical levels: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (each a distinct tier; a model only accepts the levels its menu advertises). Also accepts per-model menu option ids (e.g. `deep` → mapped wire value), same as `/effort`. Works in TUI and headless. |
| `--permission-mode <MODE>` | Permission mode. `bypassPermissions` enables always-approve via this flag (see [22-permissions-and-safety.md](22-permissions-and-safety.md)); for deny-by-default use `defaultMode` in `.claude/settings.json`. |
| `--allow <RULE>`        | Permission allow rule with glob patterns (repeatable). Works in TUI and headless. |
| `--deny <RULE>`         | Permission deny rule with glob patterns (repeatable). Works in TUI and headless. |
| `--prompt-json <JSON>`  | Prompt as JSON content blocks                         |
| `--prompt-file <PATH>`  | Prompt from a file                                    |
| `--verbatim`            | Send prompt exactly as given                          |
| `--no-auto-update`      | Disable update checks for this session                |
| `--sandbox <PROFILE>`   | Sandbox profile for filesystem/network access         |

> **Note:** `--tools`, `--disallowed-tools`, `--max-turns`, and `--agents` are headless-only flags. If used in the interactive TUI, a warning is printed and the flag is ignored. `--reasoning-effort`/`--effort`, `--permission-mode`, `--allow`, and `--deny` work in both modes. For more flags (agents and worktrees), see [Additional Headless Flags](#additional-headless-flags).

### Tool Filtering

Use `--tools` to restrict the agent to an explicit set of tools (allowlist), or `--disallowed-tools` to remove specific tools from the default set (denylist). Both accept comma-separated tool names.

Tool names are internal tool IDs (e.g. the shell tool is `run_terminal_cmd`, not `bash`).

```bash
# Only allow read-only tools
grok -p "Explain this codebase" --tools "read_file,grep,list_dir"

# Remove web access and file editing
grok -p "Review this code" --disallowed-tools "web_search,web_fetch,search_replace"

# Remove shell access
grok -p "Review this code" --disallowed-tools "run_terminal_cmd"
```

`--disallowed-tools` also supports special `Agent` entries to control subagent spawning:

| Entry                  | Effect                                  |
| ---------------------- | --------------------------------------- |
| `Agent`                | Block all subagent spawning             |
| `Agent(explore)`       | Block the `explore` subagent type only  |
| `Agent(explore, plan)` | Block multiple specific types           |

```bash
# Prevent the agent from spawning any subagents
grok -p "Fix this bug" --disallowed-tools "Agent"

# Block only the explore subagent
grok -p "Refactor this module" --disallowed-tools "Agent(explore)"
```

`--tools` preserves the selected agent profile's injection policy: stock profiles inject enabled optional tools before applying the allowlist, while curated profiles remain strict. The final toolset retains requested tools plus always-on MCP meta-tools. When both flags are present, `--disallowed-tools` wins.

### Permission Rules (`--allow` / `--deny`)

Permission rules control whether specific tool invocations are auto-approved, denied, or require user confirmation. Unlike `--disallowed-tools` (which removes tools entirely), permission rules leave tools available but gate their execution.

Rules use `ToolPrefix(glob_pattern)` syntax:

| Prefix        | What it controls                   |
| ------------- | ---------------------------------- |
| `Bash(...)`   | Shell command execution            |
| `Edit(...)`   | File editing (path glob)           |
| `Write(...)`  | File writing (path glob)           |
| `Read(...)`   | File reading (path glob)           |
| `Grep(...)`   | Search operations (path glob)      |
| `WebFetch(...)` | URL fetching (glob or `domain:host`) |
| `MCPTool(...)` | MCP tool invocations              |

For path rules (`Read`, `Edit`, `Write`, `Grep`), `*` is a single-level wildcard and `**` is recursive. For `Bash` rules, `*` matches any characters including spaces. A bare prefix without parentheses matches all invocations of that type, and `Bash(cmd:*)` is equivalent to prefix matching on `cmd`. See [22-permissions-and-safety.md](22-permissions-and-safety.md#rule-matching-reference) for the full matching semantics.

```bash
# Deny shell commands matching "rm*"
grok -p "Clean up this project" --deny "Bash(rm*)"

# Allow npm commands, deny sudo
grok -p "Set up the project" --allow "Bash(npm*)" --deny "Bash(sudo*)"

# Allow all bash commands (auto-approve without prompting)
grok -p "Build the project" --allow "Bash"
```

`--allow` and `--deny` can be repeated. Deny rules take precedence over allow rules.

---

## Output Formats

Headless mode supports three output formats, selected with `--output-format`.

### plain (default)

Human-readable text, suitable for direct display or piping:

```
Here's a summary of the codebase...
```

### json

A single JSON object emitted after the response completes: response text,
stop reason, session ID, request ID (plus `thought` when reasoning is present).
When the prompt reached the model, the same object also carries spend fields
(`usage`, `num_turns`, `modelUsage`, cost).

```json
{
  "text": "Here's a summary of the codebase...",
  "stopReason": "EndTurn",
  "sessionId": "abc123",
  "requestId": "xyz789",
  "num_turns": 7,
  "usage": {
    "input_tokens": 7210,
    "cache_read_input_tokens": 41000,
    "output_tokens": 1893,
    "reasoning_tokens": 412,
    "total_tokens": 50103
  },
  "modelUsage": {
    "grok-build": {
      "inputTokens": 7210,
      "outputTokens": 1893,
      "cacheReadInputTokens": 41000,
      "modelCalls": 7,
      "costUSD": 0.01268905
    }
  },
  "total_cost_usd": 0.01268905,
  "total_cost_usd_ticks": 126890500
}
```

Usage notes:

- `usage` sums tokens for the prompt, including subagents that finished
  before turn end (also under their own `modelUsage` keys). Compaction and
  other side-model calls are excluded.
- **Token field policy (headless result / `end` / error spend):**
  - `usage.input_tokens` and `modelUsage.*.inputTokens` are **uncached only**.
  - `cache_read_input_tokens` / `cacheReadInputTokens` are cache hits.
  - `total_tokens` is full input + output (includes cache):
    `total_tokens = input_tokens + cache_read_input_tokens + output_tokens`.
  - ACP `_meta.usage.inputTokens` (PromptUsage) is still the **full** prompt
    sum; only the headless projector subtracts cache. Prefer headless fields
    for spend automation.
- `num_turns` counts main-agent model rounds recorded on the prompt ledger
  (tool-loop rounds that reported usage). Subagent sampler calls do not
  increase it. Per-model call counts (including subagents) stay on
  `modelUsage.*.modelCalls`. This is the same counter family as `--max-turns`,
  not a guarantee of exact equality when rounds lack usage or hit gates.
- `total_cost_usd` appears only when the server reported a **complete** cost.
  Absence means unreported or incomplete, never free. Cost is stamped for
  API-key traffic today; pool/OAuth paths often omit it until the server
  stamps cost. When some calls lacked cost, `cost_is_partial` is true and
  **all** cost floats are omitted (`total_cost_usd` and every
  `modelUsage.*.costUSD`) so consumers cannot sum model rows into a fake
  complete bill.
- `total_cost_usd_ticks` is the same value in exact integer ticks
  (1 USD = 10^10 ticks) and appears under the same conditions. Use it for
  billing reconciliation: summing per-invocation ticks matches the server's
  usage export exactly, which float dollars cannot guarantee.
- When subagent usage could not be applied, nested subagent usage was incomplete,
  or the success-path drain timed out (up to 120s on the turn task),
  `usage_is_incomplete` is true and cost floats are omitted the same way
  (token totals may under-count subagents). Cancel snapshots without that long
  drain and marks incomplete while subagents are still live. Incomplete with
  no recorded tokens emits only `usage_is_incomplete` (no zero `usage` object).
- A prompt that never reached the model omits the spend fields.

The `sessionId` field is useful for resuming the conversation later.

On failure, Grok emits an error object (process exit non-zero). Prompt-level
failures may also include frozen spend fields when usage was recorded:

```json
{"type":"error","message":"Couldn't start session: ..."}
```

### streaming-json

Newline-delimited JSON events emitted in real time. Each line is a self-contained JSON object with a `type` field:

```json
{"type":"text","data":"Here's"}
{"type":"text","data":" a summary"}
{"type":"thought","data":"Analyzing the directory structure..."}
{"type":"end","stopReason":"EndTurn","sessionId":"abc123","requestId":"xyz789","usage":{...},"num_turns":7,"modelUsage":{...}}
```

Event types:

| Type       | Description                                                    |
| ---------- | -------------------------------------------------------------- |
| `text`     | A chunk of the agent's response text                            |
| `thought`  | Internal reasoning (thinking tokens)                            |
| `end`      | Final event with metadata and spend fields when available       |
| `error`    | An error occurred (carries `message`, and spend fields if any)  |

`end` is always the last event. Spend fields on `end` match the json object
shape (snake_case uncached `input_tokens`, safe cost floats).

Grok may also emit `max_turns_reached` and `auto_compact_*` events; treat the list as non-exhaustive and switch on `type`.

---

## Session Management in Headless Mode

By default, each `grok -p` invocation creates a fresh session. To maintain context across calls, use session flags.

### Named Sessions (`-s`)

To carry context across headless calls, use `-r/--resume` or `-c/--continue`. Use `-s/--session-id` only for a **new** session with a **UUID** (errors if not a UUID or already in use under the target directory). Older hidden `-s` upsert/resume behavior is gone — use `-r`/`-c` to continue. With `-r`/`-c`, `-s` requires `--fork-session`:

```bash
# Start a headless session and capture its ID
grok -p "Review the changes in this PR" --output-format json | jq -r '.sessionId'

# Continue in the same session
grok -p "Now check for security issues" --resume "<id>"

# Optional: create with a client-chosen UUID (must not already exist)
grok -p "hello" --session-id "$(uuidgen | tr '[:upper:]' '[:lower:]')" --output-format json
```

> **Note:** `-s/--session-id` creates a new session only (valid UUID; errors if already in use). Use `-r` to resume.

### Resume (`-r`)

The `-r/--resume` flag resumes a specific session by ID, or by title for the current directory when the value is not an ID, ignoring letter case (a sole manually renamed match wins among duplicates; remaining duplicates error with their IDs; UUID-shaped values always take the ID path — scripts should prefer IDs). It errors if the session does not exist:

```bash
# Get the session ID from a previous JSON response
grok -p "Remember: the secret number is 42" --output-format json
# Output includes "sessionId": "abc123"

# Resume that exact session
grok -p "What's the secret number?" --resume abc123
```

### Continue (`-c`)

The `-c/--continue` flag continues the most recent session in the current working directory:

```bash
grok -p "Continue where we left off" -c
```

### Extracting Session IDs

Use `--output-format json` and parse the `sessionId` field:

```bash
grok -p "Hello" --output-format json | jq -r '.sessionId'
```

---

## Piping Input and Output

Headless mode works naturally with Unix pipes and redirection.

### Standard Output

```bash
# Pipe output to a file
grok -p "Generate a README" > README.md

# Parse JSON output with jq
grok -p "List files" --output-format json | jq -r '.text'
```

### Standard Input

Headless mode does not read piped stdin into the prompt. Pass external content through command substitution or `--prompt-file`:

```bash
# Include git diff as context via command substitution
grok -p "Write a concise commit message for these changes:

$(git diff --staged)"

# Or read the prompt from a file
grok --prompt-file ./prompt.txt
```

---

## CI/CD Integration Examples

### Automated Code Review

```bash
grok -p "Review changes for bugs and security issues." \
  --output-format json --yolo | jq -r '.text' > review.md
```

### Pre-Commit Hook

```bash
grok -p "Review staged changes for obvious bugs. Reply OK if fine, or list issues." \
  --yolo --output-format json | jq -r '.text' | grep -q "^OK" || exit 1
```

### Batch Processing

```bash
for file in src/*.js; do
  grok -p "Migrate $file from CommonJS to ES modules." --yolo
done
```

---

## Scripting Patterns

### Python Wrapper

Grok's headless mode can be wrapped as an OpenAI-compatible chat completion API:

```python
import asyncio
import json
import os

class GrokChat:
    """Simple OpenAI-compatible wrapper using headless mode."""

    def __init__(self, cwd="."):
        self.cwd = cwd
        self.env = {**os.environ}

    def _build_cmd(self, prompt, model, stream):
        return ["grok", "-p", prompt, "-m", model, "--cwd", self.cwd,
                "--output-format", "streaming-json" if stream else "json",
                "--yolo"]

    async def create(self, messages, model="grok-build", stream=False):
        prompt = messages[-1]["content"] if len(messages) == 1 else "\n".join(
            f"{m['role']}: {m['content']}" for m in messages
        )
        cmd = self._build_cmd(prompt, model, stream)

        if stream:
            return self._stream(cmd)

        proc = await asyncio.create_subprocess_exec(
            *cmd, env=self.env, stdout=asyncio.subprocess.PIPE
        )
        stdout, _ = await proc.communicate()
        data = json.loads(stdout.decode()) if stdout else {"text": ""}
        return {
            "choices": [{
                "message": {"role": "assistant", "content": data.get("text", "")},
                "finish_reason": "stop"
            }]
        }

    async def _stream(self, cmd):
        proc = await asyncio.create_subprocess_exec(
            *cmd, env=self.env, stdout=asyncio.subprocess.PIPE
        )
        async for line in proc.stdout:
            if not line.strip():
                continue
            event = json.loads(line)
            if event.get("type") == "text":
                yield {"choices": [{"delta": {"content": event["data"]}}]}
            elif event.get("type") == "end":
                yield {"choices": [{"delta": {}, "finish_reason": "stop"}]}


async def main():
    client = GrokChat(cwd=".")
    response = await client.create(
        [{"role": "user", "content": "What files are here?"}]
    )
    print(response["choices"][0]["message"]["content"])

asyncio.run(main())
```

### Shell Script

```bash
#!/bin/bash
# Run a code review and exit with failure if issues are found

RESULT=$(grok -p "Review this PR for bugs. Output JSON with 'issues' array." \
  --output-format json --yolo | jq -r '.text')

ISSUE_COUNT=$(echo "$RESULT" | jq '.issues | length' 2>/dev/null || echo "0")

if [ "$ISSUE_COUNT" -gt 0 ]; then
  echo "Found $ISSUE_COUNT issues"
  echo "$RESULT" | jq '.issues[]'
  exit 1
fi

echo "No issues found"
```

---

## Fully Automated Runs with --yolo

The `--yolo` flag enables always-approve mode (the same mode as `--permission-mode bypassPermissions` and `--always-approve`), auto-approving tool executions (file writes, command execution, etc.) without prompting for confirmation. Explicit `deny` rules and `PreToolUse` hooks still apply, and administrators can disable the mode via `requirements.toml` (see [22-permissions-and-safety.md](22-permissions-and-safety.md)). This is required for unattended automation:

```bash
# Format all files without asking
grok -p "Format all files" --yolo

# Run tests and fix failures
grok -p "Run the tests and fix any failures" --cwd ~/projects/my-app --yolo
```

**Use `--yolo` with care.** It grants the agent full autonomy to modify files and run commands. Only use it in trusted environments or with well-scoped prompts.

---

## Environment Variables for Headless

Key environment variables that affect headless mode:

| Variable                        | Description                                                   |
| ------------------------------- | ------------------------------------------------------------- |
| `XAI_API_KEY`        | API key for authentication (required when no browser login)   |
| `GROK_HOME`                    | Override config directory (default: `~/.grok`)                |
| `GROK_LOG_FILE`                | Path to a log file (used verbatim as the path; works in headless and TUI, honors `RUST_LOG`) |
| `RUST_LOG`                     | Log level filter (e.g. `debug`). Headless logs to stderr.     |

For CI environments without browser access, set `XAI_API_KEY` with an API key from [console.x.ai](https://console.x.ai):

```bash
export XAI_API_KEY="xai-..."
grok -p "Run the test suite" --yolo
```

---

## Exit Codes

| Code | Meaning                              |
| ---- | ------------------------------------ |
| `0`  | Success -- prompt completed normally |
| `1`  | Error -- authentication failure, network error, or runtime error |
| `130` | Interrupted by SIGINT (Ctrl+C)                                   |
| `143` | Terminated by SIGTERM                                            |

---

## Authentication for Headless Environments

For headless use, authenticate with one of:

- **`XAI_API_KEY`** — simplest for CI. See [Environment Variables](#environment-variables-for-headless) above.
- **`grok login --device-auth`** (or `--device-code`) — no browser needed on the target machine.
  See [Authentication > Device Code Flow](02-authentication.md#device-code-flow).
- **`grok login`** — browser-based OAuth2 on machines with a GUI.

If you've previously logged in, cached credentials are used automatically.

---

## Tips

- Headless mode starts a **fresh session by default**. Use `-r/--resume` or `-c/--continue` to maintain context across calls.
- The `--output-format json` response always includes a `sessionId` you can use with `--resume` for follow-up calls.
- Combine `--yolo` with `--rules` to set guardrails: `grok -p "..." --yolo --rules "Never delete files"`.
- For debugging, raise the log level and capture stderr: `RUST_LOG=debug grok -p "..." 2> debug.log`.

---

## Project Root Discovery

When Grok starts, it discovers the project root by walking upward from `--cwd`
(or the current directory) until it finds a `.git` directory.

Note: If `--cwd` is nested inside a large repository (such as a monorepo),
Grok discovers that repository as the project root and scopes its discovery (AGENTS.md, skills, git history) to it, which can make
startup slow. Point `--cwd` at the specific subproject you want to work in to keep
the scope small.

---

## File Locations

Grok stores data in `~/.grok` (override with `GROK_HOME`; see [Environment Variables for Headless](#environment-variables-for-headless)):

| Path                     | Contents                              |
| ------------------------ | ------------------------------------- |
| `config.toml`            | User configuration                    |
| `auth.json`              | Cached OAuth2/API credentials         |
| `version.json`           | Version cache for update checks       |
| `sessions/`              | Session transcripts (SQLite)          |
| `memory/`                | Cross-session memory store            |
| `logs/`                  | Internal log files (for example `unified.jsonl`) |
| `logs/mcp/`              | MCP server logs                       |
| `skills/`                | User skill definitions                |
| `personas/`              | User-scoped agent personas            |
| `crash/`                 | Crash reports                         |
| `trace-exports/`         | Session trace exports                 |
| `worktrees/`             | Git worktree metadata                 |

### Read-Only `~/.grok`

For containers or CI, mount `~/.grok` read-only:

- Pre-populate `auth.json` or use `XAI_API_KEY`
- Session persistence fails silently (ephemeral)
- Update checks log a warning and skip

```bash
export XAI_API_KEY="xai-..."
export GROK_DISABLE_AUTOUPDATER=1
grok -p "..." --no-auto-update
```

---

## Update Check Suppression

| Method                          | Scope     |
| ------------------------------- | --------- |
| `--no-auto-update`              | Session   |
| `GROK_DISABLE_AUTOUPDATER=1`    | Process   |
| Non-TTY stderr (auto-detected)  | Automatic |
| `[cli] auto_update = false`     | Persistent|

`GROK_DISABLE_AUTOUPDATER` set to a falsy value (`0`, `false`, `off`, `no`, or empty, any
case) counts as not set. The agent SDKs
inject `GROK_DISABLE_AUTOUPDATER=1` for the non-leader agents they spawn (a falsy value in
the SDK's isolation env keeps updates on), and the stdio agent skips its background update
unless it runs from the managed install (`$GROK_HOME/bin/grok`).

Update messages go to **stderr**. Stdout stays clean for `--output-format json`. See also [Environment Variables for Headless](#environment-variables-for-headless).

---

## Additional Headless Flags

These flags supplement the [Command-Line Options](#command-line-options) table above. Flags already listed there (`--prompt-json`, `--prompt-file`, `--verbatim`, `--sandbox`, `--no-auto-update`) are not repeated here.

| Flag                          | Description                                       |
| ----------------------------- | ------------------------------------------------- |
| `--agent <NAME>`              | Agent name or definition file path                |
| `--agents <JSON>`             | Inline subagent definitions as JSON               |
| `--system-prompt-override`    | Override the agent's system prompt                |
| `--no-plan`                   | Disable plan mode                                 |
| `--no-subagents`              | Disable subagent spawning                         |
| `--no-memory`                 | Disable cross-session memory                      |
| `--disable-web-search`        | Disable web search and fetch tools                |
| `--no-alt-screen`             | Run inline (no alternate screen)                  |
| `--worktree [NAME]`           | Start session in a new git worktree               |
| `--ref <REF>` / `--worktree-ref <REF>` | Branch/tag/commit to base the worktree on (with `--worktree`) |

---

## Interrupted Headless Runs

On SIGINT/SIGTERM:

- Session state saved up to the last completed tool call
- File modifications by tools are **not rolled back**
- Exit code is **130** for SIGINT (`128 + 2`) and **143** for SIGTERM (`128 + 15`); CI pipelines can distinguish these from a normal error (exit code `1`)
- Resume: `grok -p "continue" --resume "<id>"` or `grok -p "continue" --continue`

See [Session Management in Headless Mode](#session-management-in-headless-mode) for details on named sessions and the `-s`/`-r`/`-c` flags.
