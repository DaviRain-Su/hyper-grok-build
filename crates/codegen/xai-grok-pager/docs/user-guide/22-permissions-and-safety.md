# Permissions and Safety Controls

Grok can read files, search code, edit files, and run shell commands. The permission system controls what the agent is allowed to do. You can combine several independent layers: permission rules, permission modes, hooks, and the OS-level sandbox.

This guide explains how a tool call is authorized, how to configure permission rules from the CLI, native configuration, or Claude settings, and how to use `PreToolUse` hooks for allow lists that apply in every mode.

---

## How a Tool Call Is Authorized

When the model requests a tool, the following checks happen in order:

1. **`PreToolUse` hooks**. A hook can deny a tool call before any other check. A hook that allows a call does not skip the checks below; it only declines to deny. See [10-hooks.md](10-hooks.md).

2. **Permission rules** (from configuration files or `--allow`/`--deny` flags)
   - A matching `deny` rule rejects the call. `deny` wins over every other rule.
   - A matching `ask` rule prompts you, including for file reads, searches, and shell commands that would otherwise be auto-approved.
   - A matching `allow` rule approves the call.

3. **Remembered grants**. Per-command approvals you saved from earlier prompts apply here, scoped to the current project. An existing grant can satisfy an `ask` rule instead of re-prompting. Commands on the [dangerous list](#dangerous-commands) prompt again rather than using a remembered prefix. See [Interactive Approvals](#interactive-approvals-and-where-they-persist).

4. **Built-in auto-approvals**. Read-only tools and a fixed set of read-only shell commands run without prompting (see below).

5. **Prompt policy** (set by the [permission mode](#permission-modes)): prompt you, auto-approve, or auto-deny the call.

Always-approve mode (`bypassPermissions`) short-circuits this pipeline after step 2: `deny` rules, hooks, and `ask` rules that match a shell command's segments still apply, but remembered grants (including remembered "never allow" entries) are not consulted, and `ask` rules on non-shell tools do not prompt.

---

## Operations That Never Prompt by Default

The operations below are treated as read-only and run without prompting, in every mode including `dontAsk`, unless a matching `deny` rule or a hook blocks them. An `ask` rule forces a prompt for file reads, searches, and shell commands (see [How a Tool Call Is Authorized](#how-a-tool-call-is-authorized)).

### Read-Only Tools

- `read_file`
- `list_dir`
- `grep` (content search)
- `web_search`
- `todo_write`
- `get_command_or_subagent_output` / `wait_commands_or_subagents` / `kill_command_or_subagent` (subagent control)
- Invoking skills

### Read-Only Shell Commands

After splitting chained commands (on `&&`, `||`, `;`, and pipes), the following commands are recognized as read-only when they appear as the primary command. This list is word-boundary matched, so `ls` does not match `lsof` or `less`. (Your own `Bash(...)` rules match differently; see [Rule Matching Reference](#rule-matching-reference).)

**Filesystem (read-only viewing):**
- `ls`, `cat`, `pwd`, `date`, `whoami`, `hostname`, `uptime`, `ps`
- `head`, `tail`, `wc`, `sort`, `uniq`, `tr`, `cut`

**Git (read-only):**
- `git status`, `git branch`, `git log`, `git diff`, `git ls-files`, `git show`, `git rev-parse`

**Search and inspection:**
- `grep`, `rg` (not `rg --pre` / `rg --pre=…`, which spawn a preprocessor per file)

**Kubernetes (read-only):**
- `kubectl get`, `kubectl logs`, `kubectl describe`

> **Note:** `tee` is not on this list because it can write its input to arbitrary files. `cargo check` is not on this list because it compiles and runs `build.rs`, proc-macros, and any `build.rustc-wrapper` from the repo (in Ask mode it therefore prompts; Auto mode may still heuristic-allow `cargo` as a project code runner). `sort --compress-program=…` (including unique long-option abbreviations), `git -c` / `--config-env` overrides, and a git command whose local/worktree config installs an executable hook (`core.fsmonitor`, a `diff.*.command`/`textconv`/`external` driver, or a shell `alias.<safe-subcommand> = !…`) raise a request-level floor and prompt rather than auto-approve, unless the user granted that exact full script or YOLO is on.

These checks apply per segment. In a command like `ls && rm -rf /`, the `ls` segment is recognized as read-only, but the `rm` segment is not on the list. In `default` mode the `rm` segment prompts; under `dontAsk` it is denied.

---

## Permission Modes

The prompt policy is named by one of these modes:

| Mode                | Behavior                                                                 | Typical Use                     |
|---------------------|--------------------------------------------------------------------------|---------------------------------|
| `default`           | Prompt for anything not pre-approved                                     | Daily interactive use           |
| `dontAsk`           | Deny anything without an explicit allow rule or built-in auto-approval   | Headless, CI, high-security     |
| `bypassPermissions` | Auto-approve tool calls (`deny` rules, hooks, and shell `ask` rules still apply) | Trusted environments    |
| `acceptEdits`       | Auto-approve file edits (`search_replace`, `write`, etc.)                | "Accept edits" workflows        |
| `plan`              | Accepted for compatibility; plan sessions are a separate feature (see [19-plan-mode.md](19-plan-mode.md)) | Structured planning sessions |

### Setting the Mode

The mode is set by `defaultMode` in `.claude/settings.json` (see [Claude Code Compatibility](#3-claude-code-compatibility-claudesettingsjson)). `dontAsk`, `acceptEdits`, and `bypassPermissions` change the prompt policy from there; `default` and `plan` keep standard prompting.

The `--permission-mode` CLI flag applies `bypassPermissions` (always-approve) and `default`; an explicit flag value always wins over a mode set in configuration. Passing `dontAsk`, `acceptEdits`, or `plan` to the flag is accepted but does not enable that policy; set those through `defaultMode` instead.

In headless runs (`-p`), a tool call that would prompt is cancelled and reported to the model instead of waiting for input. For deny-by-default in automation, set `defaultMode: "dontAsk"`.

### Disabling Always-Approve Mode

Administrators can turn always-approve (`bypassPermissions` / `--always-approve`) off so it cannot be enabled from the CLI, the TUI toggle, or the `/always-approve` command. Set the dedicated key in `requirements.toml`:

```toml
[ui]
disable_bypass_permissions_mode = true   # default: false. true = locked off.
```

Do not use `permission_mode` for this; it is a user-switchable default, not a lock. The legacy `[ui] yolo = false` key in `requirements.toml` also disables the mode, for backward compatibility; in `config.toml` the same key remains a togglable preference.

The user-level `~/.grok/requirements.toml` is under the user's control, so a developer can remove the lock by editing that file. For enforcement that users cannot override, deploy the setting in the root-owned system file `/etc/grok/requirements.toml`.

> **Note:** Grok honors the permission rules in Claude Code's `managed-settings.json`, but not its `disableBypassPermissionsMode` lock. To disable always-approve in Grok, use `requirements.toml` as shown above.

---

## Configuring Permissions

Grok reads permission rules from three compatible sources. Rules from all sources are merged into one set; a rule's effect depends on its action (`deny` > `ask` > `allow`), not on which file it came from.

### Where Permission Rules Live (Scopes)

Permission rules can be global (all projects), project-scoped (one repository), or personal to you within a project:

| Scope | File | Shared with teammates |
|-------|------|-----------------------|
| Global (all projects) | `~/.grok/config.toml` | No |
| Project (committed) | `<project>/.grok/config.toml` | Yes (commit it) |
| Project (personal) | `<project>/.claude/settings.local.json` | No (gitignore it) |
| Interactive grants | Stored internally by Grok, per project | No |

Notes on scoping:

- Grok discovers a `.grok/config.toml` at every directory level from the repository root down to your working directory, so a subdirectory can add rules on top of the repo root's.
- Rules from all scopes are merged into one rule set; `deny` > `ask` > `allow` applies across scopes, so a global `deny` cannot be overridden by a project `allow`.
- Grok has no native `config.local.toml`. For personal, uncommitted rules in a project, use `.claude/settings.local.json`; Grok reads it directly (see [Claude Code Compatibility](#3-claude-code-compatibility-claudesettingsjson)).
- Interactive "Always allow" decisions are stored outside the repository, scoped to the project (see [Interactive Approvals](#interactive-approvals-and-where-they-persist)).

To stop prompts for a specific command in one project, add a narrow allow rule to that project's `.grok/config.toml` (or `.claude/settings.json`):

```toml
[permission]
allow = ["Bash(cargo test *)", "Bash(npm run build)"]
```

This approves only the listed commands. Always-approve mode, by contrast, approves all tool calls.

### 1. CLI Flags

```bash
grok -p "Review the API changes" \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)' \
  --allow 'Read' \
  --allow 'Grep' \
  --deny 'Bash(rm -rf *)'
```

`--allow RULE` and `--deny RULE` can be repeated and are always enforced.

Rule syntax examples:
- `Bash(git *)` — any command starting with `git `
- `Bash(npm run build)` — exact command (or prefix)
- `Bash(git commit:*)` — the `cmd:*` suffix form, equivalent to prefix matching on `git commit`
- `Read(src/**)` — read access under `src/`
- `Edit(**/*.rs)` — edit any Rust file
- `Grep` — all grep operations
- `MCPTool(my-server__*)` — MCP tools from a specific server

See [Rule Matching Reference](#rule-matching-reference) for the exact matching semantics, including how chained commands and wildcards are evaluated.

### 2. Native Configuration (`~/.grok/config.toml` and `.grok/config.toml`)

```toml
[permission]
rules = [
  { action = "allow", tool = "bash", pattern = "git *" },
  { action = "allow", tool = "bash", pattern = "gh *" },
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "bash", pattern = "rm -rf *" },  # block a dangerous pattern
  { action = "ask",   tool = "edit" },
]
```

The structured `tool` field accepts the lowercase names `bash`, `read`, `edit`, `grep`, `mcp`, `webfetch`, and `websearch`, corresponding to the tool classes in [Tool Names](#tool-names).

Because `deny` always wins, you cannot combine these `allow` rules with a catch-all `deny` on `bash` to mean "only allow git/gh"; a `deny tool = "bash"` rule would block `git` and `gh` too. For deny-by-default, use `defaultMode: "dontAsk"` in `.claude/settings.json` or a `PreToolUse` hook (below).

Rules from the global `~/.grok/config.toml` and every project `.grok/config.toml` (from the repo root down to your working directory) are merged into one rule set, alongside any `.claude/settings.json` rules.

Managed configuration deployed by your organization also contributes `[permission]` rules: the system `/etc/grok/managed_config.toml`, and a user-level copy that Grok maintains automatically at `~/.grok/managed_config.toml`. Managed rules merge like rules from any other source, with two properties specific to managed `allow` rules: your own `deny` and `ask` rules win over a managed `allow` (severity ordering), and a catch-all managed `allow` is ignored when always-approve is locked off. For rules that users cannot edit away, use the root-owned system `/etc/grok/requirements.toml`.

Permission rules from every source are read once, when a session starts. Changes apply to the next session.

The native `[permission]` section also accepts the compact `allow` / `deny` / `ask` string-array form, using the same rule strings as the `--allow` / `--deny` flags and `.claude/settings.json`:

```toml
[permission]
deny = [
  "Read(/Users/you/private/**)",
  "Edit(/Users/you/private/**)",
  "Bash(rm -rf *)",
]
allow = [
  "Bash(git *)",
  "Bash(gh *)",
]
```

`deny` always wins over `allow` (evaluation is `deny` > `ask` > `allow`), regardless of order or source. To block reads of paths outside your project at the OS level as well, combine deny rules with the `strict` sandbox profile (see [18-sandbox.md](18-sandbox.md)).

### 3. Claude Code Compatibility (`.claude/settings.json`)

Grok reads `~/.claude/settings.json` and `~/.claude/settings.local.json`, plus the project-level `<project>/.claude/settings.json` and `settings.local.json` (walking up to the repo root). The native `.grok` source for permission rules is `config.toml`, described in the section above.

Example:

```json
{
  "permissions": {
    "defaultMode": "dontAsk",
    "allow": [
      "Read",
      "Grep",
      "Bash(git *)",
      "Bash(gh *)"
    ],
    "deny": [
      "Bash(rm -rf *)"
    ]
  }
}
```

Supported `defaultMode` values are `default`, `acceptEdits`, `bypassPermissions`, `dontAsk`, and `plan`. Grok reads `defaultMode` from its canonical location under `permissions`; a top-level `defaultMode` is also accepted when the nested key is absent.

`permissions.allow`, `permissions.deny`, and `permissions.ask` entries are translated into native rules and then matched with the semantics in the [Rule Matching Reference](#rule-matching-reference). Translation notes:

- Rules for MCP tools must use the `MCPTool(server__tool)` form; the `mcp__server__tool` form never matches (see [MCP Rules](#mcp-rules)).
- Rules naming an unrecognized tool, and parameter rules such as `Agent(model:opus)`, are skipped with a warning rather than failing the load.
- `permissions.additionalDirectories` is parsed but not supported.

You can import existing Claude settings interactively with **Ctrl+I** ("Import Claude settings").

---

## Rule Matching Reference

This section defines exactly how rules are matched.

### Bash Rules

A `Bash(...)` pattern matches a command in either of two ways:

- **Prefix**: the command starts with the pattern text, compared character for character. There is no word-boundary requirement, so `Bash(git)` matches `gitleaks` as well as `git status`. Include a trailing space and wildcard (`Bash(git *)`) to require the prefix to be a whole word.
- **Glob**: the pattern matches the whole command as a glob. `*` can appear at any position and matches any characters, including spaces and slashes, so `Bash(git * main)` matches `git checkout main`. `?` and `[...]` are also supported.

Matching is case-sensitive. Leading whitespace in the command is trimmed before matching; nothing else is normalized.

A trailing `:*` suffix on a Bash rule is stripped to a plain prefix: `Bash(git commit:*)` becomes prefix `git commit`. Because prefixes have no word boundary, a `deny` written as `Bash(sed:*)` also blocks commands such as `sed-custom`.

**Chained commands.** Grok parses each command like a shell and splits it on `&&`, `||`, `;`, `|`, and newlines. The rule actions treat segments differently:

- `deny` and `ask` rules are checked against every segment, and against the whole string. One denied segment rejects the entire command.
- `allow` rules are checked against the whole command string only. `Bash(git *)` therefore auto-approves `git status && rm -rf /`, because the full string starts with `git `. Pair narrow allow rules with `deny` rules for the patterns you want to block.

Commands that cannot be split into simple segments (subshells, command substitution `$(...)`, backticks, background `&`, control flow) prompt as a single unit when Bash restrictions are configured.

Segment-level checks (`deny` and `ask` rules, remembered grants, and the read-only command list) strip environment-variable prefixes such as `RUST_LOG=debug`, and peel a fixed set of process wrappers (`timeout`, `nice`, `ionice`, `chrt`, `stdbuf`, `env`) so that `deny` and `ask` rules match either the wrapped or the inner command. `deny` and `ask` rules are also checked inside inline scripts passed to `bash -c`. Other wrappers, including `sudo`, `xargs`, and `nohup`, are not peeled; write rules that include them explicitly. `allow` rules do not get this treatment: they match the command string as written, so a leading environment assignment or wrapper keeps an `allow` rule from matching and the command prompts instead.

### Dangerous Commands

A built-in list (`rm`, `chmod`, `chown`, `chgrp`, `chattr`, `pkill`, `kill`, `killall`, `git push`) prompts even when a segment is covered by a remembered command prefix or the read-only command list. An explicit `allow` rule in configuration does approve them, and always-approve mode auto-approves them like any other command; use `deny` rules to block them unconditionally. Review rules like `Bash(rm *)` carefully before adding them as allow rules.

### Read, Edit, and Grep Rules

Path patterns are globs matched against the path string the tool was called with:

- `*` and `?` do not cross `/`; `**` does. `Read(src/*)` matches `src/main.rs` but not `src/nested/mod.rs`; use `Read(src/**)` for the whole tree.
- A bare filename matches only that exact string. Use `**/.env` to match `.env` at any depth.
- There are no anchor prefixes: a leading `//` or `~/` in a pattern is treated as literal glob text. Write absolute-path patterns or `**/` patterns instead.
- Paths are matched as given, without canonicalization. Whether a path is absolute or relative depends on how the tool was invoked, so patterns intended as boundaries should cover both forms (for example both `/repo/secrets/**` and `secrets/**`).
- `Read` rules also govern `grep` searches; `Grep(...)` rules match only grep.

`Read` and `Edit` deny rules additionally apply to file paths that shell commands touch (for example `cat` or `sed` on a denied path), including literal inline scripts passed to `bash`, `sh`, `dash`, `zsh`, or `ksh` with `-c`; that shell-level check also resolves symlinks. The direct `read_file`/`search_replace` tool checks do not resolve symlinks. For OS-level enforcement that covers every process, combine deny rules with the sandbox ([18-sandbox.md](18-sandbox.md)).

### MCP Rules

`MCPTool(...)` patterns match the full Grok tool name in `server__tool` form, with glob support: `MCPTool(linear__*)` matches every tool from the `linear` server. Grok tool names carry no `mcp__` prefix, so a rule written as `mcp__server__tool` never matches an MCP call; write `MCPTool(server__tool)` instead.

### WebFetch Rules

- `WebFetch(domain:example.com)` matches that host and every subdomain (`api.example.com`), case-insensitively, ignoring a leading `www.`. Wildcards are not supported inside `domain:` patterns.
- A pattern without the `domain:` prefix globs against the entire URL: `WebFetch(https://api.example.com/*)`.

### Tool Names

Recognized tool names: `Bash`, `Read` (and `NotebookRead`), `Edit` (and `Write`, `NotebookEdit`), `Grep` (and `Glob`), `MCPTool`, `WebFetch`, `WebSearch`. A bare `*` rule matches every tool. Globs are not supported in the tool-name position.

Rules naming an unrecognized tool (for example `Agent(model:opus)`) are skipped with a warning rather than failing the load.

### Evaluation Order

Rules from every source are merged into one set and evaluated by severity, not order: any matching `deny` rejects, otherwise any matching `ask` prompts, otherwise any matching `allow` approves. When no rule matches, the request falls through to the built-in auto-approvals and then the prompt policy, as described in [How a Tool Call Is Authorized](#how-a-tool-call-is-authorized).

---

## Interactive Approvals and Where They Persist

When a tool call requires approval, the permission prompt offers these choices:

- **Allow once**: approve this single invocation.
- **Reject once**: reject it, optionally with a message back to the model.
- **Enable always-approve mode**: approves all future tool calls, not just the one being prompted.
- **Allow all edits this session**: shown for file edits. This grant is held in memory only and does not survive a restart.

### Per-Command "Always Allow"

A narrower set of options remembers just the specific command, MCP tool, or web-fetch domain being prompted, for example "Always allow `cargo test`". These rows are off by default. Enable them with:

```toml
# ~/.grok/config.toml
[ui]
remember_tool_approvals = true
```

With the gate enabled, prompts gain:

- **`Always allow: <command>`**, which persists an allow for the command prefix.
- A matching "never allow" row, which persists a deny the same way.
- Equivalent "always allow" rows for MCP tools and web-fetch domains.

The remembered prefix is limited to a short form of the command: read-only commands persist just their listed prefix (for example `git status`, not the full argument list), and other commands persist a short leading prefix. The prompt shows exactly what will be remembered before you confirm. Commands on the [dangerous list](#dangerous-commands) prompt again rather than using a remembered prefix.

### Persistence Is Per Project

Interactive grants are stored in Grok's own state directory under your home directory, scoped to the directory you launched Grok from. A grant made in one project never applies in another, grants are not written into the repository, and they are not meant to be hand-edited.

Interactive grants are personal, per-machine state. For an allowlist you can review in code review and share with teammates, use declarative rules in the project's `.grok/config.toml` instead.

---

## Restricting Bash to Specific Commands with a Hook

A `PreToolUse` hook can enforce an allow list on the `Bash` tool that applies in every permission mode. Hooks are evaluated before the permission system; a hook deny stops the call, and a hook allow falls through to the normal permission checks (so your `deny` rules still apply).

> **Note:** Hooks fail open. If a hook script crashes, times out, or is missing, the tool call proceeds as if the hook had allowed it, and the failure is reported in the UI. A hook used as a security boundary must handle its own errors, and must account for chained commands, as the example below does. See [10-hooks.md](10-hooks.md).

### Example: Allow Only `git` and `gh`

**`~/.grok/hooks/git-gh-only.json`**

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "git-gh-only.sh",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

**`~/.grok/hooks/git-gh-only.sh`**

```bash
#!/bin/sh
# Allow only git and gh commands, including within chained commands.

set -eu

deny() {
  echo '{"decision": "deny", "reason": "'"$1"'"}'
  exit 2
}

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.toolInput.command // empty')

[ -n "$CMD" ] || deny "Empty command is not allowed"

# Normalize '&&' and '||' to ';' so chains can be checked segment by
# segment, then reject constructs this script cannot inspect.
CMD=$(echo "$CMD" | sed 's/&&/;/g; s/||/;/g')
case "$CMD" in
  *'$('*|*'`'*|*'&'*|*'>'*|*'<'*) deny "Substitution, background, and redirection are not permitted" ;;
esac

# Split on the separators and require every segment to start with git or gh.
echo "$CMD" | tr ';|' '\n\n' | while IFS= read -r SEGMENT; do
  SEGMENT=$(echo "$SEGMENT" | sed 's/^[[:space:]]*//')
  [ -n "$SEGMENT" ] || continue
  case "$SEGMENT" in
    git\ *|git|gh\ *|gh) ;;
    *) deny "Only git and gh commands are permitted. Blocked segment: $SEGMENT" ;;
  esac
done
```

```bash
chmod +x ~/.grok/hooks/git-gh-only.sh
```

This hook denies every `Bash` command unless each chained segment starts with `git` or `gh`, and rejects command substitution, backgrounding, and redirection outright because it cannot verify what they execute. It works in every permission mode.

For hook installation, the JSON format, the trust model for project hooks, and other events, see [10-hooks.md](10-hooks.md), which also contains a complementary "block dangerous patterns" example.

---

## Example Configurations

### Headless git and gh Only (CI and Automation)

```bash
grok -p "Implement the feature using only git and GitHub CLI" \
  --allow 'Read' \
  --allow 'Grep' \
  --allow 'Bash(git *)' \
  --allow 'Bash(gh *)'
```

Install the `git-gh-only` hook above to deny every other `Bash` command. For deny-by-default on all tools, also set `{"permissions": {"defaultMode": "dontAsk"}}` in `.claude/settings.json`.

### Read-Only Code Reviewer

```toml
# .grok/config.toml
[permission]
rules = [
  { action = "allow", tool = "read" },
  { action = "allow", tool = "grep" },
  { action = "deny",  tool = "edit" },
  { action = "deny",  tool = "bash" },
]
```

### Interactive Development

Use `default` mode plus narrow `Bash(...)` allow rules for the commands you run most (`git`, `cargo test`, `rg`, and similar).

---

## Combining with the Sandbox

Permissions control what the model is allowed to request. The OS-level sandbox (see [18-sandbox.md](18-sandbox.md)) controls what the process can do even after a command is approved.

Recommended combination for untrusted code:

1. `dontAsk` plus narrow allow rules, or a restrictive hook
2. `--sandbox strict` or a custom profile
3. Project trust plus review of any `SessionStart` hooks

---

## Managing Permissions in the TUI

- Permission decisions appear in the transcript.
- The `/always-approve` command toggles always-approve mode; other modes are set through `defaultMode` (see [Setting the Mode](#setting-the-mode)).
- With `[ui] remember_tool_approvals = true`, permission prompts include per-command "Always allow" options that persist for the current project only. See [Interactive Approvals](#interactive-approvals-and-where-they-persist).
- To manage hooks and plugins, run `/hooks` or `/plugins` (on most terminals, **Ctrl+L** also opens the Extensions modal; on VS Code, Cursor, Windsurf, and Zed, `Ctrl+L` is mid-turn interject instead). See [10-hooks.md](10-hooks.md).

---

## Best Practices

1. **Prefer narrow patterns.** `Bash(git *)` grants less access than a bare `Bash` allow rule.
2. **Combine layers.** `dontAsk`, narrow allow rules, a restrictive hook, and the sandbox each restrict independently.
3. **Review project configuration from unfamiliar sources.** Project permission rules in `.grok/config.toml` and `.claude/settings.json`, including `allow` rules, apply without a separate trust prompt. Review them, and any project hooks, before working in an unfamiliar checkout (see the security notes in [10-hooks.md](10-hooks.md)).
4. **Test your policy.** With `defaultMode: "dontAsk"` set (or your `PreToolUse` hook installed), run representative commands and confirm what is blocked.
5. **Treat the read-only command list as a convenience, not a security boundary.**

---

## See Also

- [10-hooks.md](10-hooks.md) — Hook authoring guide
- [14-headless-mode.md](14-headless-mode.md) — Headless flags, including permission-related ones
- [18-sandbox.md](18-sandbox.md) — OS-level isolation profiles
- [05-configuration.md](05-configuration.md) — Native `config.toml` structure
