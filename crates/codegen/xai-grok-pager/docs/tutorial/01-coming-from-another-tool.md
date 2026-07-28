# Coming from Claude, Cursor, Codex, or OMP?

Fear not — Grok Build can continue sessions from all four tools. It also reads
supported project conventions and compatibility sources where the corresponding
vendor integration is available.

## Picked up automatically

- **Rules & instructions** — `AGENTS.md` (the Codex/OpenCode convention),
  `CLAUDE.md` (including nested ones), and `*.md` rules under
  `.claude/rules/` and `.cursor/rules/`.
- **Skills & custom commands** — `~/.claude/skills/`, `~/.claude/commands/`,
  `~/.cursor/skills/`, and their project-level twins. Flat command `.md`
  files become slash commands here too.
- **MCP servers** — from `~/.claude.json`, `.cursor/mcp.json`, and project
  `.mcp.json`.
- **Hooks** — from `.claude/settings.json`, including matcher aliases like
  `Bash`, so most hooks run unchanged.

## One-step import

**`/import-claude`** scans your `~/.claude` settings — permissions, env
vars, MCP servers, hooks — and shows a checkbox preview; confirming
writes the items you selected into your `.grok` config. Re-run it anytime.

## Pick up where you left off

The native foreign-session picker and recent-session hint support Claude Code,
Codex, Cursor, and OMP. Selecting one dispatches the matching
**`/resume-claude`**, **`/resume-codex`**, **`/resume-cursor`**, or
**`/resume-omp`** integration.

## Check what was discovered

Run **`grok inspect`** in a repo to see every rules file, skill, and MCP
server Grok picked up, tagged with where it came from. Each compat source
can be toggled in the matching `[compat.claude]`, `[compat.cursor]`,
`[compat.codex]`, or `[compat.omp]` config section.

And a few things you might have missed elsewhere: `/btw` asks a side
question without interrupting the current task, and `/rewind` restores
actual file snapshots, not just chat history.

*Go deeper: `/docs Project Rules (AGENTS.md)`, `/docs Skills`, or `/docs MCP Servers`*
