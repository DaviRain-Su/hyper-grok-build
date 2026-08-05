# Subagents and Personas

Subagents are independent child sessions that handle tasks in parallel. Each subagent has its own context window, so the main agent can delegate work (research, implementation, testing, and code review) without consuming its own context. A subagent reports a summary back to the parent when it finishes.

Subagents are enabled by default.

---

## Agents vs Personas

Agents and personas both customize behavior, but they operate at different levels:

| | **Agents** | **Personas** |
|---|---|---|
| **What they configure** | The whole session: model, tools, prompt mode, system prompt | A behavioral overlay added to a subagent's prompt |
| **Scope** | Primary session or subagent | Subagents only |
| **How you set them** | At startup, or with agent definitions (`.md` files in `.grok/agents/` or `~/.grok/agents/`) | In `config.toml` (`[subagents.personas]`) or `.toml` files under `.grok/personas/`; applied during subagent resolution |
| **What they control** | Model, tool availability, prompt body, skills | Tone, output format, task focus, and input/output contracts |
| **Who edits them** | You -- create, delete, or toggle them in the agents modal or by editing files | You -- define custom personas in config or files; bundled personas are read-only |
| **Examples** | `grok-build`, `explore`, `plan` | `researcher`, `concise` |

An agent defines the session itself. A persona shapes how a subagent behaves within a session. A subagent always runs as an agent type (for example, `general-purpose`), and resolution can layer a persona on top.

Manage both in the agents modal. Open it with `/config-agents` (alias `/agents`), or open the Personas tab directly with `/personas`. The modal has two tabs: **Agents** and **Personas**.

---

## Disabling Subagents

Disable subagents with an environment variable or the config file:

```bash
export GROK_SUBAGENTS=0              # Environment variable
```

```toml
# ~/.grok/config.toml
[subagents]
enabled = false
```

---

## How Subagents Work

When the main agent identifies work to delegate, it calls the `spawn_subagent` tool to start a child session. The child runs with:

- Its own context window, independent of the parent
- A toolset determined by its agent type and optional capability mode
- Optional persona instructions applied during resolution

The parent receives the child's output -- usually a summary -- when the child finishes.

---

## Built-in Agent Types

The `spawn_subagent` tool accepts a `subagent_type` parameter that selects the child's role:

| Type              | Description                                          |
| ----------------- | ---------------------------------------------------- |
| `general-purpose` | Default type. Full-capability agent for any task.    |
| `explore`         | Fast read-only research agent. Reads files, lists directories, and searches code; it has no shell or editing tools. |
| `plan`            | Read-only planning agent. Inspects the codebase and maintains a planning todo list; it has no shell or editing tools. |
| `oracle`          | Deep-analysis advisor. Read-only; the working agent consults it when stuck, debugging a complex issue, or weighing approaches, then acts on its recommendation. Pin it to a stronger model for best effect. |

### Bundled specialist agents

Release packages also ship markdown agents under `bundled/agents/` (installed to
`~/.grok/bundled/agents/`). They appear as **bundled** subagent types (lower
priority than project `.grok/agents/` or `~/.grok/agents/`):

| Type | Description |
| ---- | ----------- |
| `scout` | Fast read-only scout with a compressed handoff map (`Summary` / `Files` / `Architecture`). Prefer for “where is X?” mapping; overlaps `explore`. |
| `reviewer` | Read-only pre-merge review; P0–P3 findings on the patch only. |
| `security-reviewer` | Read-only security pass; source→sink evidence, no speculative issues. |
| `librarian` | External library/API research from **installed or cloned source**, with citations. |
| `designer` | UI/UX implementation and review; design-system first, read-write. |

Spawn them with `spawn_subagent` / Task using `subagent_type` equal to the name
above. Override or disable via project/user agents of the same name, or
`[subagents.toggle]`.

Project- or user-defined agents can add new types or shadow these built-ins by name.

### Consulting the Oracle

The oracle pattern pairs a **working model** with a **stronger analysis model**: the main agent keeps working on its own model, and when it hits a problem that needs deeper reasoning it spawns the `oracle` subagent, reads its recommendation, and continues. The oracle is read-only at the toolset level, so it can inspect the repo but never edits it.

**Pin a stronger model or the pattern does nothing useful.** Without a pin, Oracle **inherits the session model**. If the main session is already on that model (for example Grok), Oracle is the same model with a fancier prompt — not Amp-style deep think.

Pin Oracle with `/agents` → select `oracle` → `m`, or in `config.toml`:

```toml
[subagents.models]
oracle = "openai/gpt-5.6"   # example; use any stronger slug you have credentials for
```

Typical setup: cheap/fast model on the main session, frontier model on `oracle` only.

**When the main agent should consult Oracle** (it decides via `spawn_subagent`; Hyper does not auto-run Oracle every turn like some cloud products):

- Same test or error still failing after repeated edit attempts
- Unclear root cause, architecture trade-off, or high-risk change
- You ask it to rethink, review the approach, or get a second opinion

If the working model keeps thrashing instead of calling Oracle, **say so in the chat** — there is no `/oracle` slash. For example: “用 oracle 看为什么测试还红”, “ask oracle why this still fails”, or “spawn oracle on the root cause”. Pin a stronger model first so the consult is worth the cost.

**Observability.** Two guards keep the pin honest:

- If an `oracle` subagent spawns and its resolved model is the **same** as the session's, a toast warns: *"Oracle is using the same model as this session — pin a stronger one via `/agents` → `oracle` → `m`."*
- `/doctor` reports a recommendation when Oracle has no `[subagents.models]` pin at all — and, in the TUI, when the pin equals the session model.

The built-in Oracle is bounded by default: 12 model/tool-use rounds, 40 tool calls, and 180 seconds total wall-clock time. Thirty seconds are reserved for finalization. Near a limit, Grok Build tells Oracle to stop investigating and return its best structured recommendation; if it ignores the warning, the hard tool/time limit cancels it. The Oracle response contract includes findings, evidence, alternatives, risks, a verification handoff, confidence, and a final recommendation.

Design notes (pin warnings, chat-trigger obedience): repository `docs/design-oracle.md`.

Custom or shadowing agent definitions can use the same YAML frontmatter fields:

```yaml
maxTurns: 12
maxToolCalls: 40
timeoutSecs: 180
finalizeGraceSecs: 30
```

All values must be greater than zero. `maxTurns` is the existing model/tool-loop limit; the other fields are optional. Agents without these fields remain unbounded, except when they inherit a parent `maxTurns` limit.

---

## Personas

A persona is a named behavioral overlay. Its instructions are injected into the subagent's conversation as a `<system-reminder>`, which shapes tone, output format, and task focus without changing the subagent's agent type, model, or tools.

Define personas in `config.toml` or in `.toml` files:

```toml
[subagents.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build discovers file-based personas from these locations, in priority order:

- `.grok/personas/*.toml` (project)
- `~/.grok/personas/*.toml` (user)
- The bundled personas directory (lowest priority)

Each file defines one persona, and the file name (without the extension) becomes the persona name. Inline `config.toml` personas take precedence over files. Only `.toml` files are discovered.

Manage personas in the Personas tab of the agents modal (`/personas`). Personas you define (user or project files) are editable; bundled personas are read-only, but applying a model to one writes a customized copy to `~/.grok/personas/` that shadows the bundled definition.

To set a persona's model from the TUI, select it in the Personas tab and press `m` — the same model picker as agent pins opens (type to filter, `↑`/`↓` to choose, Enter to apply, **inherit** to clear). Only models with configured credentials are offered. The choice is written as the `model` key in the persona's `.toml` file. Project personas (`.grok/personas/`) are re-discovered at every spawn, so their model applies immediately; user personas (`~/.grok/personas/`) load at session start, so their model applies in new sessions.

> **Note:** Grok Build applies personas through subagent resolution and roles, not through a `spawn_subagent` parameter. The main agent does not pass a persona name when it spawns a child.

### Persona Fields

| Field               | Description                                                          |
| ------------------- | ------------------------------------------------------------------- |
| `instructions`      | Inline instruction text applied as the persona layer.               |
| `instructions_file` | Path to an instruction file, loaded at spawn time and merged after `instructions`. |
| `description`       | Short summary shown in the persona catalog. Falls back to the first paragraph of `instructions`. |
| `inputs` / `outputs`| Declared input and output contract (see below).                     |
| `model`             | Model override applied when the persona is used.                    |
| `reasoning_effort`  | Validated reasoning effort (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or `ultra`). |
| `default_isolation` | Default isolation mode (`none` or `worktree`).                      |

### Input/Output Contracts

A persona can declare the inputs it expects and the outputs it produces. The parent agent reads these to know what context to supply and what artifacts to expect. This lets you chain personas, so one persona's output file becomes the next persona's input:

```toml
[[subagents.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagents.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

Each field has a `name`, an `io_type` (defaults to `file`), a `required` flag, and a `description`.

### Persona Resolution

When a persona applies, Grok Build resolves the effective model and reasoning effort once at the spawn boundary, in this order (highest priority first):

1. Explicit spawn-time override
2. Role default
3. Persona default
4. Agent-definition default
5. Parent session

Isolation follows the same first four layers but defaults to `none` (no worktree) rather than inheriting from the parent session. Capability mode is intentionally stricter: the explicit request, role, and agent-definition modes are intersected as security ceilings. A caller can narrow access, but cannot widen an `oracle`, `explore`, or `plan` agent beyond read-only. The clamp runs after default tool assembly, and built-in read-only agents do not inherit unclassified MCP tools; `explore` and `oracle` keep their exact curated toolsets.

If a persona is requested but cannot be resolved -- it is not found, has no instructions, or its `instructions_file` is unreadable -- the spawn fails. Reasoning-effort values are parsed into a fixed enum during configuration loading, so misspellings fail early instead of silently reaching a provider.

---

## Spawning Subagents

The main agent calls the `spawn_subagent` tool. Its parameters:

| Parameter         | Description                                                       |
| ----------------- | ---------------------------------------------------------------- |
| `prompt`          | The full task prompt for the subagent.                           |
| `description`     | A short label for the task (3-5 words).                          |
| `subagent_type`   | The agent type to launch. Defaults to `general-purpose`.         |
| `background`       | Run the subagent in the background and return immediately with a subagent ID. Defaults to `true`. |
| `capability_mode` | Restrict the subagent's tools: `read-only`, `read-write`, `execute`, or `all`. |
| `isolation`       | `none` (shared workspace, the default) or `worktree` (isolated git worktree). |
| `resume_from`     | Continue a completed subagent's conversation. Pass its subagent ID. |
| `cwd`             | Working directory for the subagent. Mutually exclusive with `isolation: worktree`; ignored when `resume_from` is set (the resumed child inherits its source's directory). |

When you run a subagent in the background, retrieve its result later with `get_command_or_subagent_output`.

---

## Capability Modes

A capability mode is an optional, coarse filter on a subagent's tools:

| Mode         | Read | Write | Execute | Description                                  |
| ------------ | ---- | ----- | ------- | -------------------------------------------- |
| `read-only`  | Yes  | No    | No      | Read, search, and inspect (also web search and LSP); no file edits or shell. |
| `read-write` | Yes  | Yes   | No      | Read, plus create, edit, delete, and move files. No shell. |
| `execute`    | Yes  | No    | Yes     | Read, plus run shell commands and background tasks. No file edits. |
| `all`        | Yes  | Yes   | Yes     | Unrestricted tool access.                    |

If you omit `capability_mode`, the subagent uses its agent type's toolset. The built-in `explore`, `plan`, and `oracle` types are enforced read-only and have no shell or editing tools; `general-purpose` ships the full toolset.

---

## Context Inheritance

### resume_from

The `resume_from` parameter lets a new subagent continue where a completed subagent left off, which is useful for multi-stage workflows:

1. Spawn a research subagent to investigate a problem.
2. Spawn a second subagent with `resume_from` set to the first subagent's ID, so it picks up with the full research context.

The new subagent inherits the source's transcript, tool state, and model; its system prompt and tools are re-rendered from the current agent definition. The source must be completed (not running), belong to the current session, and use the same agent type. If you changed that agent type's model pin and want the new model, start a fresh child without `resume_from` and hand over the needed context in its prompt.

### MCP inheritance

Subagents inherit the parent session’s **already-connected** MCP servers by default. That includes local stdio/HTTP servers and plugin-sourced agents (for example `my-plugin:reviewer`). The child discovers and calls those tools with `search_tool` / `use_tool` the same way the parent does.

Control inheritance with agent frontmatter `mcpInheritance`:

| Value | Effect |
| ----- | ------ |
| `all` (default if omitted) | Inherit every parent-connected MCP server |
| `none` | Inherit no parent MCP servers |
| `named: [server, …]` | Inherit only the listed server names |
| `except: [server, …]` | Inherit all parent servers except the listed names |

Example:

```yaml
---
name: research-only
description: Read MCP tools but not internal connectors
tools: search_tool, use_tool, Read
mcpInheritance:
  except:
    - internal-tools
---
```

**Plugin agents** inherit parent MCP the same way. For security they still cannot:

- Declare their own `mcpServers` in agent frontmatter (ignored with a warning)
- Declare hooks in agent frontmatter
- Set `permissionMode: bypassPermissions`

Plugin-bundled MCP servers (plugin `.mcp.json`) still attach to the **parent/session** after the plugin is trusted — they are not a child-only frontmatter declaration. See [Plugins](09-plugins.md) and [MCP Servers](07-mcp-servers.md).

---

## Isolation: Worktree Mode

For tasks that modify files, run a subagent in an isolated git worktree with `isolation: worktree`. This keeps the child's edits from conflicting with the parent's:

- The subagent works in its own copy of the working tree.
- Its changes stay isolated from the parent until you merge them.
- The subagent's result includes the worktree path.

Grok Build manages worktrees through the `x.ai/git/worktree/*` extension methods, including an apply operation that merges changes back into the main working directory.

---

## Configuration

### Per-Type Toggles and Model Overrides

Disable specific agent types, or route them to a different model:

```toml
[subagents.toggle]
explore = true                       # default -- omit to keep enabled
plan = false                         # disable the plan subagent

[subagents.models]
explore = "grok-build"               # route explore to a specific model
```

Per-type model overrides apply for any parent. Without an override, a subagent inherits the parent's model.

You can also manage model pins from the TUI: open `/agents`, select an agent, and press `m`. A model picker opens listing the models you have credentials for — type to filter, choose with `↑`/`↓`, and press Enter to pin. Pick **inherit** (the first row) to clear the pin and follow the session model again. Pinned agents show `→ <model>` in the list and a `pinned — [subagents.models]` note in the expanded detail. The TUI writes the pin to `~/.grok/config.toml`, refreshes the live routing map, and waits for acknowledgement before releasing input. It then applies to the next **fresh** subagent spawn without a restart; a `resume_from` continuation intentionally keeps its source model.

### Per-Type Reasoning Effort

Pin an effort level per agent type — e.g. a cheap model for `explore`, a deep-thinking one for `oracle`:

```toml
[subagents.effort]
oracle = "high"      # none, minimal, low, medium, high, xhigh, max, ultra
explore = "low"
```

Precedence (highest first): an explicit per-spawn override, then a role's `reasoning_effort`, then a persona's, then this `[subagents.effort]` pin, then the agent definition's own `effort:` field, and finally the parent session's effort. Pins that name an unknown level are ignored with a log warning. `/agents` shows pinned effort next to the model pin (`effort: <level>` in the row, and a `pinned — [subagents.effort]` note in the expanded detail); editing is via `config.toml` for now.

### Custom Roles and Personas

Define custom roles with their own capability and model defaults:

```toml
[subagents.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-build"
prompt_file = ".grok/prompts/researcher.md"
```

Define custom personas with behavioral instructions:

```toml
[subagents.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".grok/personas/concise.md"  # or load from a file
```

Grok Build also discovers roles from `.grok/roles/*.toml` and personas from `.grok/personas/*.toml`. Inline `config.toml` definitions take precedence over files.

---

## The Tasks Pane (TUI)

Grok Build shows running and finished work in side panes on the agent screen:

- Press `Ctrl+G` to toggle the tasks pane, which lists active and completed subagents and background commands with their status.
- Press `Ctrl+T` to toggle the separate todo pane.

To view the available agent types and personas, open the command palette with `Ctrl+P` and choose **Manage Agents** (`/config-agents`).

Subagents appear at the top of the tasks pane in their own collapsible "Subagents" group. Running rows show elapsed time (and the wall-clock limit when configured), live tool calls against their limit, and current context tokens. Finished rows switch to cumulative model usage and show trustworthy reported cost when available. Runtime-driven endings are labeled with reasons such as `tool limit`, `time limit`, or `budget finalized`.

---

## Viewing Subagents in the TUI

Subagents appear in several places in the interactive TUI:

### Scrollback (parent conversation history)

When a subagent is spawned, a compact lifecycle block is added to the *parent's* scrollback:

- `Subagent running: "do the thing" (Implementer · grok-3) — Thinking`
- Or for background subagents: `Subagent started: "..."`

While running, the block shows a live activity suffix (e.g. "Running: cargo test", "Compacting", "Retrying (2/3)") pulled from the child's turn tracker. The bullet animates (or is colored) according to state.

Press **Enter** (or Ctrl-F) on the block to open the subagent's full transcript.

For blocking subagents the single entry updates its bullet color when the child finishes. For background ones, a follow-up `Subagent completed/failed/cancelled in Xs: "..."` block is appended.

### Tasks pane (Ctrl+G)

As noted above — grouped under "Subagents", with spinners, elapsed times, and quick access to kill or inspect.

### Fullscreen framed view (the child transcript)

When you open a subagent (from a scrollback block or the tasks pane), the parent view is replaced by a bordered frame containing the child's full transcript:

- Title bar inside the frame: status icon (spinner / ✓ / ✗), label + bold description + model, optional "resumed"/"forked" badge, live activity · elapsed time, and [✗] close button.
- The child's own scrollback, thinking, tool calls, and (limited) prompt area render inside the frame.
- Subagent views are largely observational — you generally cannot send new top-level prompts directly to them the way you can a parent session.

Use `q`, `Esc`, or click the close button to pop back to the parent view. The parent's scrollback continues to show the subagent's status.

---

## Virtual paths: `agent://`, `history://`, `conflict://`

The `read_file` tool (and `grep` when given a virtual path) understands a few **session-local** schemes:

| URL | Meaning |
|-----|---------|
| `agent://<subagent_id>` | Last successful subagent output (`output.json`) |
| `history://` | Roster of subagents for this parent session |
| `history://<subagent_id>` | Concise transcript / final output for that subagent |
| `conflict://` | List registered merge-conflict regions |
| `conflict://N` | Full marker block for conflict N |
| `conflict://N/ours` (also `theirs`, `base`, `both`) | One side of the conflict |

Register conflicts by reading a normal file with the `:conflicts` suffix (e.g. `src/foo.rs:conflicts`). Resolve with `search_replace` / write-style edit targeting `conflict://N` and content `@ours`, `@theirs`, `@base`, or free text.

## TTSR-lite (stream rules)

Optional mid-stream course correction (default **off**). Enable with `GROK_TTSR_ENABLED=1` or in `~/.grok/config.toml`:

```toml
[features]
ttsr = true
```

Then place rules under `.grok/rules/*.md` with frontmatter:

```yaml
---
name: no-todo
condition: "(?i)\\bTODO\\b"
interruptMode: always
---
Do not leave bare TODOs; implement or file a tracked issue.
```

When the live assistant draft matches `condition`, the stream is cancelled, the rule body is injected once as a system reminder, and the model continues with one automatic retry budget.

## DAP debug stub (`dap_debug`)

The `dap_debug` tool is registered as a **stub** for future Debug Adapter Protocol sessions (`status`, `launch`, `attach`, `control`, `breakpoints`, `evaluate`). Every op currently returns a structured “not implemented” response — use a local debugger via the shell until an adapter process is wired. Reserved config: `[features] dap_debug = true`.

## Collab scaffold

Multi-user collab is config-only for now (no relay process):

```toml
[collab]
enabled = false
relay_url = "wss://collab.example/ws"
web_url = "https://collab.example"
display_name = "alice"
```

## Agent hub (peer messaging)

Within one parent session tree, **Main** and live depth-1 subagents share an in-process **agent hub** bus. Peers can message each other without routing every note through the parent’s switchboard.

The model-facing tool is `agent_hub` (distinct from the workspace remote hub). Ops:

| Op | Behavior |
|----|----------|
| `list` | Roster of peer ids, labels, and `running` / `gone` status |
| `send` | Fire-and-forget prose message to `to` (from `list`); max 8 KiB |
| `inbox` | Drain this peer’s mailbox (non-blocking) |
| `wait` | Block until mail arrives or `timeout_ms` (capped at 30s) |

Rules of thumb:

- Call `list` before `send`; never invent peer ids.
- Prefer plain prose; do not use the hub for questions tools can answer (`grep`, `read_file`, build).
- Mailbox depth is 32 (oldest dropped); soft rate limit is 20 sends per minute per sender.
- Delivery may wake a live peer via interjection; finished peers show as `gone` and reject new sends.
- Nesting is still depth one: children cannot spawn children, but they **can** use `agent_hub` with Main and sibling peers.

Specialists that ship read-only toolsets (`explore`, `oracle`, bundled scout/reviewer-style agents) include `agent_hub` so coordination stays available without shell or edit tools.

---

## Depth Limits

Only the top-level session spawns subagents. A subagent cannot spawn its own subagents: the maximum nesting depth is one. If a subagent calls `spawn_subagent`, the call fails with a depth-limit error. This keeps the agent tree flat and prevents runaway spawning.

---

## When to Use Subagents

**Good use cases:**

- Researching a codebase while the parent continues other work
- Running tests in parallel while the parent implements changes
- Reviewing generated changes before you commit them
- Delegating independent tasks that do not depend on each other

**When not to use:**

- Simple tasks that the parent can handle directly
- Tasks that require tight back-and-forth with the user, since a subagent runs autonomously and isn't suited to interactive exchanges
- Tasks where the context setup cost exceeds the parallelism benefit
