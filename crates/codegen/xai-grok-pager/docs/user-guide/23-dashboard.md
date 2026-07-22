# Agent Dashboard

The Agent Dashboard is a centralised, agent-native overview of every
top-level session you have in flight — your local sessions and forks
— grouped by state, with peek, attach, and dispatch from one screen.
Subagents are not listed here: they run under their parent session,
which already shows when work is in flight.

---

## Opening the dashboard

Three entry points, all opening the same view:

- **`grok dashboard`** — launches the TUI directly into the dashboard.
- **`/dashboard`** (aliases **`/agents-dashboard`**, **`/sessions`**) — open
  from inside an active session.
- **Ctrl+\\** — same as the slash command, two keystrokes. Configurable
  in `~/.grok/config.toml` under `[keybindings]` like every other shortcut.

---

## What you see

```
 Grok Build · Dashboard — 4 agents · 2 awaiting
▌● reviewer · audit token flow    Awaiting your input            2m
 ● implementer · fix login bug    Running: cargo test           12m
 ⋅ refactor · feat/login          Responding…                   24m
 ○ housekeeping                   idle                           1h
 ● implementer · add login tests  8 tools · 1.2k tok            14m
╭─────────────────────────────────────────────────────────────────╮
│ ❯ Dispatch a new agent                                          │
╰─ dispatch ──────────────────────────────────────────────────────╯
 ↑/↓ select (peek) · Enter open · Ctrl+R rename · Ctrl+T pin · Ctrl+X stop · ? help · Esc new
```

Each row is a top-level agent (subagents aren't shown — they run under
their parent). Rows are sorted by state (Needs input → Working → Idle →
Inactive → Completed → Failed) so same-state rows sit adjacent, or by
working directory (toggle with `Ctrl+G`). **Inactive** holds roster-only
sessions — idle/dormant sessions owned by other pager processes that
haven't been loaded in this one — so **Idle** stays focused on the
sessions you're actively cycling between. Because it's background noise,
**Inactive starts collapsed** (expand it with `→` / click — see below).

To keep the **Idle** group scannable, only the most recent idle agents
stay visible — the 8 freshest, plus any active within the last hour. The
rest fold into a **"N more"** row (marked with a `+` / `-` toggle) at the
bottom of the group; select it and press `Enter` / `→` (or click it) to
reveal them all, and `←` to re-fold. The Idle header always shows the true total. Folding is
suspended while a filter or search is active (so every match shows).

The state icon matches Grok Build's sibling views (
`tasks_pane`):

- `⋅`/`:`/`⸬`/`⁙` — animated spinner for **Working** rows.
- `●` — filled circle for **Needs input**, **Completed**, **Failed**,
  **Blocked**. Colour communicates the state (yellow / green / red /
  amber).
- `○` — hollow circle for **Idle** and **Inactive** rows.

A row stays in **Working** while it has live background work even if its
turn has finished — a running background task, a `monitor`, or an active
scheduled `/loop`. The activity line says what's running (e.g.
`1 monitor · 2 loops still running`), since each can wake the agent for a
new turn.

There are no inline group headers — the sort order keeps same-state
rows adjacent and the per-row dot+colour communicates which group
each row belongs to (matching other session lists).

The dispatch input shares the same `PromptWidget` chrome as the
agent view's prompt (rounded box, `❯` prefix, accent border, info
line). Pressing `Ctrl+/` flips it into **search mode**: the `❯`
prefix becomes a yellow `Search:` and whatever you type live-filters
the row list instead of being dispatched.

---

## Keybindings

| Key | Action |
| --- | --- |
| `↑` / `↓`, `j` / `k` | Navigate rows AND section titles (selecting a row opens its peek panel) |
| `→` / `←` (on a section title) | Expand / collapse the section (shows / hides its rows); `l` / `h` in vim mode |
| `Enter` (on a section title) | Toggle the section collapsed / expanded |
| `Enter` (empty reply) | Open the selected agent's conversation full-screen (details view) |
| `Ctrl+S` | Send the peek reply AND open the agent (or dispatch + attach a new session) |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the reply / dispatch input (multiline compose) |
| `1`–`9` | Answer a pending permission / ask question (when the peek shows options) |
| `Enter` (typed reply) | Send / queue the reply to the selected agent |
| `/` | Types a literal `/` into the prompt |
| `Ctrl+/` | Toggle search mode (live-filter the rows) |
| `Ctrl+R` | Rename selected row |
| `Ctrl+T` | Pin / unpin |
| `Ctrl+G` | Toggle grouping (state ↔ directory) |
| `Ctrl+X` | Stop / kill (two presses within 2s to close a session) |
| `Shift+↑` / `Shift+↓` | Reorder pinned rows |
| `Esc` | Step back one level: cancel search → close peek (clear reply draft, then unselect) → clear filter → **unfocus the dispatch input** (so `↑`/`↓`, `j`/`k` navigate the list) → unselect row (→ `[+ New Agent]`) → exit dashboard. Esc never clears your typed dispatch draft — use `Ctrl+U` / `Ctrl+C` for that |
| `Ctrl+\` | Return to the dashboard from the details view, or exit dashboard |
| `Ctrl+.` (alt: `?`) | Open the keyboard shortcuts cheatsheet. Footer advertises `?` when `Ctrl+.` cannot be delivered. Bare `?` opens help when list-focused or the draft is empty (otherwise types); `Ctrl+X` remains stop |

When grouping by state, each group has a **section title** (e.g. `Working`,
`Idle`) with a `▸`/`▾` disclosure marker. Section titles are part of the
up/down navigation: select one and press `→` to expand it (showing its rows)
or `←` to collapse it — `l` / `h` do the same when vim mode is on.
**Clicking** a section title toggles it, and **hovering**
brightens its text. Collapse state is remembered while the dashboard stays open.
The **Inactive** section starts collapsed by default each time the pager
starts; expanding it sticks until you quit.

Opening a row shows the agent's conversation in the **details view**:
a single top header row (the agent name on the left, `{i}/{n} [‹][›]
[Dashboard]` cycle/close affordances on the right) sits above the conversation,
which renders **full-width** — no bordered modal frame — so the prompt
position and overall padding match the dashboard list view. All key
presses route to the attached agent; `Esc` / `Ctrl+\\` (or the `[Dashboard]`
affordance) return to the dashboard, the `[‹]` / `[›]` chips cycle to
the previous / next agent, and the agent's shortcuts bar shows a
`Ctrl+\\: back to dashboard` hint. Quick gotcha — `Esc` only returns to
the dashboard; typing `/exit` inside the agent actually closes the
underlying session (returning to the dashboard with a "Session closed"
toast).

`Ctrl+X` in the details view is state-dependent. While a **turn is
running** it cancels the turn — the same behaviour as `Ctrl+C`,
including the keep-subagents prompt — and never touches the session
itself, so mashing it to stop a turn can't close anything. In any
other state — **idle**, a slash command in flight (commands can't
be cancelled yet), or a cancel still pending — `Ctrl+X` arms a
confirmation: the shortcuts bar flips to "press Ctrl+x again to
close this session", and a second press within 2 seconds closes the
session and returns you to the dashboard. Pressing any other key
cancels the confirmation, and a turn that starts inside the window
downgrades the confirmed press to a cancel instead of closing.
(On terminals where `Ctrl+X` doubles as the shortcuts-cheatsheet
binding, the cheatsheet stays reachable via `Ctrl+.` inside the
details view.)

For the full behavioural specification (including the registry-lookup
rules and the mouse-event intercept matrix) see plan
[§3.10](../../plan/agent-dashboard.md) "Keybindings (v1)" — the user
guide here is intentionally short and cross-references the plan as
the source of truth.

All shortcuts are registered under `When::DashboardFocused` and can be
rebound via `~/.grok/config.toml`.

---

## Dispatch input

The bottom textarea **always spawns a NEW session** — it is never a
reply target. A selected row is the overview's navigation cursor, not a
reply destination; to talk to an existing agent, open it (navigate +
`Enter`, or click) and reply inside its own view.

Enter handler:

- Free text → creates a new top-level session, seeded with the prompt.
  Text is **never** reinterpreted as a filter — a prompt may start with
  `/`, `s:`, `a:`, or `#` and still dispatches verbatim (filtering is
  the explicit `Ctrl+/` search mode). A leading `/` runs a pager-global
  slash command.
- Empty input → opens the selected row (`Attach`), or creates a new
  agent when the `[+ New Agent]` button is focused.

Press `Ctrl+S` after typing a prompt to dispatch AND attach
(jump into the new session); plain `Enter` stays on the dashboard so
you can dispatch several sessions in a row. `Shift+Enter` / `Alt+Enter`
insert a newline for a multi-line prompt — the box **grows in height**
as you add lines (up to a cap, after which it scrolls), so the whole
draft stays visible.

The dispatch input accepts any non-empty prompt; an empty /
whitespace-only prompt is ignored. Prompts above 64 KiB are rejected
with a toast.

### Focus: input bar ↔ overview list (`Tab`)

The dashboard has two focus areas — the **dispatch input bar** (typing)
and the **overview list** (navigating). `Tab` toggles between them; the
inactive input dims its border and hides its caret.

On open, focus defaults to the **overview list** when at least one agent
exists (so `↑`/`↓` / vim `j`/`k` navigate immediately). With **no**
agents, focus stays on the **dispatch input** so you can type a first
prompt right away. Either way, the `[+ New Agent]` button is the cursor
target (no agent row is pre-selected).

- **Input focused**: type to compose a new-session prompt. `↑`/`↓`
  navigate the row list when the prompt is empty (a convenience),
  otherwise move the caret. `Esc` unfocuses the input → overview list
  (your typed draft is kept) so you can navigate straight away.
- **Overview focused**: `↑`/`↓` — and, in **vim mode**, `j`/`k` — move
  between agent rows. `Enter` opens the highlighted agent (on
  `[+ New Agent]`, it sends a typed draft, else creates a new session).
  `Esc` **stays on the list** and steps back — clearing an active filter,
  then unselecting the row (→ `[+ New Agent]`), then exiting the
  dashboard. `Tab` or `i` (vim) — or any other printable key — return to
  the input.

---

## Peek panel

The peek panel is shown **by default whenever an agent row is
selected** — it **replaces** the new-session dispatch box. With no row
selected (the `[+ New Agent]` button focused, or after `Esc`), the
dispatch box returns for starting a new session. So selecting a row is
how you talk to an existing agent; deselecting is how you start a new
one.

The panel shows, top to bottom, a header (the **last response type** —
`Thinking` / `Thought` / `Response` / `Edit` / `Read` / `Bash` / … — on
the left, **time** on the far right), the most recent response
(**word-wrapped** to fit, up to ~3 rows), and a live `❯ reply` input. A
`…` marker appears on the last row only when there's more than fits.

The selected agent's **model** and, when it's in always-approve (yolo)
mode, an **`always-approve`** flag are shown on the panel's **bottom
border** (bottom-right) — the same config-badge slot the new-session
dispatch box uses. This holds in the question / approval modes too, so
the model and approval mode are always in view while you answer. (The
dashboard list rows no longer repeat the model or an always-approve badge,
keeping the list compact.)

**`Shift+Tab` cycles the peeked agent's mode** (Normal → Plan →
Always-approve → Normal) — the same cycle as Shift+Tab inside that agent's
chat view, applied to the **live** agent (the badge updates to match).
This differs from the new-session dispatch box, where Shift+Tab only
stages the mode for the *next* agent.

Unlike the dispatch box (which only ever spawns new sessions), the
peek's reply **talks to the selected agent**:

- **Type into `❯ reply`, then `Enter`** to send. An **idle** agent
  starts the turn immediately; a **busy** agent **queues** the message
  so it sends after the current turn finishes (the same queue/drain
  behaviour as the agent view's own prompt). `Ctrl+S` replies AND
  opens the agent's detail view; `Shift+Enter` / `Alt+Enter` insert a
  newline (multiline compose) and the reply **grows in height** to fit
  the draft (up to a cap, then it scrolls).
- With an **empty** reply, `Enter` opens the agent.
- **`↑`/`↓` move the caret within the reply** once it has content (so you
  can edit a multi-line draft). While the reply is **empty** (or
  unfocused via `Tab`), `↑`/`↓` instead **switch the selected agent** —
  the panel follows the selection cursor and refreshes live, and the
  switch clears any half-typed draft so a reply can't land on the wrong
  agent. (`Tab` to the row list to navigate agents while a draft is in
  the reply.)
- **`Esc` unselects**: it first clears a typed reply, then deselects the
  row and focuses the `[+ New Agent]` button (bringing back the
  new-session input).
- **`Tab`** toggles focus between the reply input and the row list: an
  unfocused reply dims its border and hides the caret; a printable key
  re-focuses it and starts composing.
- The reply is a **full prompt editor** (the same component as the
  dispatch box and the agent prompt): pasting multi-line text folds
  into a `[Pasted: N lines]` chip with the same preview overlay and
  expand affordances as the agent prompt (`Enter` / double-click /
  paste-again), mouse click / drag place the caret and select text,
  and the usual editing chords work (word navigation, `Ctrl+A`/`Ctrl+E`,
  `Alt+Backspace`, `Ctrl+W`/`Ctrl+U`/`Ctrl+K`, undo, Shift+arrow
  selection, `Ctrl+Shift+V` inline paste).
  Typing **`@`** opens the file-context picker rooted at the **peeked
  agent's** working directory (so `@path` resolves against the agent
  you're replying to); its dropdown floats **above** the panel and
  `↑`/`↓`/`Tab`/`Enter`/`Esc` drive it while it's open.
  Dashboard chords (`Ctrl+X` stop, `Ctrl+T` pin, `Shift+↑/↓` reorder,
  …) still win over the editor while the panel is open.
- When a **permission / ask-tool question** is pending, the `❯ reply`
  row is hidden and the options are listed instead: **`↑`/`↓` move the
  highlighted option** (marked with `▸`) and **`Enter` answers** it.
  **`1`–`9`** still answer an option directly. (While answering, the
  arrows pick options rather than switching agents.)
- The **free-text row** accepts an inline typed answer (just like the
  chat panel): the permission **"No" / reject** option ("No, reject
  (type to add feedback)") and the ask-tool **"Other"** row ("Other
  (type your own answer)"). Type on it and `Enter` sends the rejection +
  message / the free-text answer.
- This also covers the agent's **Ask tool** (`AskUserQuestion`): its
  options + the "Other" row show in the peek, answered the same way.
  **Multi-question** forms are walked one question at a time — a `(i/N)`
  marker shows progress and each answer advances to the next, submitting
  on the last. (Forms with a **multi-select** question are left to the
  agent's own view — open the agent to answer those.)

The panel only renders when the terminal is tall enough; on very short
terminals the dispatch box shows even with a row selected.

---

## Search / filter (`Ctrl+/`)

Filtering lives behind an explicit **search mode** so normal typing
always dispatches. Press `Ctrl+/` to toggle it: the prompt prefix
flips from `❯` to a yellow `Search:` and every keystroke live-filters
the row list.

Inside search mode:

- `Enter` — **confirm**: keep the filter applied and return to the
  dispatch prompt (rows stay filtered; `Esc` later clears them).
- `Esc` or `Ctrl+/` — **cancel**: clear the filter and exit search.
- `↑` / `↓` — navigate the filtered rows.

The query supports the same prefixes as before (they are only honoured
*inside* search mode now):

- `a:<name>` — filter by agent label (case-insensitive substring,
  matches persona / role).
- `s:<state>` — filter by row state. Accepts `working`, `idle`,
  `completed`, `failed`, `needs-input`, `blocked` and synonyms
  (`busy`/`running`/`done`/etc.).
- `#<text>` — substring match on `#<text>` (matches the literal
  `#` in labels; reserved for future PR filtering).
- anything else — plain substring match over label + working dir.

---

## Persistence

Per-user dashboard preferences live under `[dashboard]` in
`~/.grok/config.toml`:

```toml
[dashboard]
enabled = true
grouping = "state"   # or "directory"
pinned   = ["top:<session_id>", "sub:<parent_session_id>:<child_session_id>"]
reorder  = ["top:<session_id>"]
```

Pinned/reorder entries are keyed by **session id**, not by the
per-process `AgentId(usize)`, so they survive restarts and don't
attach to whatever agent happens to share the old slot number.

Set `GROK_AGENT_DASHBOARD=0` to force-disable the feature for a single
pager invocation; the slash command and CLI subcommand will print a
friendly toast.

---

## Phase 4 (out of scope for v1)

The current dashboard lists only agents owned by **this** pager
process. The plan's Phase 4 ("supervisor / `grok --bg`") would list
sessions that survive pager exit — that's a separate roadmap and not
shipped yet.
