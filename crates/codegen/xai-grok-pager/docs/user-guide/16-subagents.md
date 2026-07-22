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
| `explore`         | Research agent. Searches, reads, greps, and runs shell commands, but does not edit files. Use it for codebase investigation. |
| `plan`            | Planning agent. Explores the codebase and produces a structured implementation plan; does not edit files. |
| `oracle`          | Deep-analysis advisor. Read-only; the working agent consults it when stuck, debugging a complex issue, or weighing approaches, then acts on its recommendation. Pin it to a stronger model for best effect. |

Project- or user-defined agents can add new types or shadow these built-ins by name.

### Consulting the Oracle

The oracle pattern pairs a fast working model with a stronger analysis model: the main agent keeps working on its own model, and when it hits a problem that needs deeper reasoning it spawns the `oracle` subagent, reads its recommendation, and continues. The oracle is read-only at the toolset level, so it can inspect the repo but never edits it.

Pin the oracle to your strongest configured model with `/agents` → select `oracle` → `m` (or `[subagents.models] oracle = "..."` in `config.toml`). Without a pin it inherits the session model, which defeats the purpose.

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
| `reasoning_effort`  | Reasoning effort applied when the persona is used.                  |
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

When a persona applies, Grok Build resolves the effective model and reasoning effort in this order, highest priority first:

1. Explicit spawn-time override
2. Role default
3. Persona default
4. Parent session

Isolation follows the same order for the first three steps but defaults to `none` (no worktree) rather than inheriting from the parent session.

If a persona is requested but cannot be resolved -- it is not found, has no instructions, or its `instructions_file` is unreadable -- the spawn fails.

---

## Spawning Subagents

The main agent calls the `spawn_subagent` tool. Its parameters:

| Parameter         | Description                                                       |
| ----------------- | ---------------------------------------------------------------- |
| `prompt`          | The full task prompt for the subagent.                           |
| `description`     | A short label for the task (3-5 words).                          |
| `subagent_type`   | The agent type to launch. Defaults to `general-purpose`.         |
| `background`       | Run the subagent in the background and return immediately with a subagent ID. Defaults to `false`. |
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

If you omit `capability_mode`, the subagent uses its agent type's toolset. The built-in `explore` and `plan` types read, search, and run shell commands but cannot edit files; `general-purpose` ships the full toolset.

---

## Context Inheritance

### resume_from

The `resume_from` parameter lets a new subagent continue where a completed subagent left off, which is useful for multi-stage workflows:

1. Spawn a research subagent to investigate a problem.
2. Spawn a second subagent with `resume_from` set to the first subagent's ID, so it picks up with the full research context.

The new subagent inherits the source's transcript, tool state, and model; its system prompt and tools are re-rendered from the current agent definition. The source must be completed (not running), belong to the current session, and use the same agent type.

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

You can also manage model pins from the TUI: open `/agents`, select an agent, and press `m`. A model picker opens listing the models you have credentials for — type to filter, choose with `↑`/`↓`, and press Enter to pin. Pick **inherit** (the first row) to clear the pin and follow the session model again. Pinned agents show `→ <model>` in the list and a `pinned — [subagents.models]` note in the expanded detail. Pins are written to `~/.grok/config.toml` and apply to the next subagent spawn — no restart needed.

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

Subagents appear at the top of the tasks pane in their own collapsible "Subagents" group.

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
