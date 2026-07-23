# Plugins

A plugin bundles skills, slash commands, agents, hooks, MCP server configurations, and LSP server configurations into one installable unit.

---

## What a plugin contains

A plugin is a directory that holds any combination of these components:

- **Skills** -- a `skills/` directory of SKILL.md files
- **Slash commands** -- a `commands/` directory of command files
- **Agents** -- an `agents/` directory of agent definitions
- **Hooks** -- a `hooks/hooks.json` file of lifecycle hooks. Plugin hooks also receive `GROK_PLUGIN_ROOT` and `GROK_PLUGIN_DATA` (see the [Hooks guide](10-hooks.md) for every environment variable passed to hooks).
- **MCP servers** -- a `.mcp.json` file of server configurations
- **LSP servers** -- a `.lsp.json` file of language server configurations

If a plugin includes a `plugin.json` manifest, the manifest can override paths or add metadata; otherwise components load from the convention directories. The manifest is optional: without one, Grok discovers the components above from their standard directories.

For example, a `team-tools` plugin might include a deploy skill, a code-review agent, pre-commit hooks, and a Linear MCP server. Install them together in one step.

## Environment variables in plugin hooks

Plugin hooks receive two environment variables beyond the standard ones set for every hook:

| Variable             | Description |
|----------------------|-------------|
| `GROK_PLUGIN_ROOT`   | Absolute path to the plugin's installed directory. |
| `GROK_PLUGIN_DATA`   | Absolute path to the plugin's writable data directory, for plugin state, caches, and logs. |

Grok sets these values and overrides any value you declare for the same key in the hook JSON's `env` map. (Grok also sets the `CLAUDE_PLUGIN_ROOT` and `CLAUDE_PLUGIN_DATA` aliases for compatibility.) See the [Hooks guide](10-hooks.md) for every environment variable passed to hooks.

---

## Plugin locations

Grok discovers plugins from these locations, in priority order:

| Location | Scope | Trust |
|----------|-------|-------|
| `_meta.pluginDirs` (`session/new` / `session/load`) | Session -- loaded for that session only | Trusted automatically |
| `--plugin-dir` (CLI flag, `grok agent`) | Process -- loaded for that agent process only | Trusted automatically |
| `.grok/plugins/` | Project -- shared with the team through version control | Requires trust |
| `~/.grok/plugins/` | User -- personal plugins for every project | Trusted automatically |
| `[plugins].paths` (config) | Custom directories you add in `config.toml` | Depends on location |

Grok also reads the `.claude/plugins/` equivalents for compatibility. When two plugins share a name, the higher-priority location wins.

The Agent SDKs load per-session plugins through `GrokOptions.plugins`, which arrives as `_meta.pluginDirs` on `session/new` and `session/load`; because the caller controls the directory, these plugins are always trusted -- their hooks and MCP servers activate without a prompt, and they never persist beyond the session. The `--plugin-dir` flag is the process-wide equivalent for direct CLI use (repeatable: `grok agent --no-leader --plugin-dir A --plugin-dir B stdio`); it applies to dedicated agent processes only and is ignored in leader mode (the shared leader discovers its own plugins).

---

## Manage plugins in the TUI

### Open the modal

| Action | Opens |
|--------|-------|
| `Ctrl+L` (from any pane; **non–VS Code family**) | Plugins tab |
| `/plugins` (any terminal; **required on VS Code family**) | Plugins tab |

The modal has five tabs: **Hooks**, **Plugins**, **Marketplace**, **Skills**, and **MCP Servers**. Switch tabs with `Tab` (forward) or `Shift+Tab` (backward). The `/hooks`, `/marketplace`, `/skills`, and `/mcps` commands each open the modal on the matching tab.

### Plugins tab

Press `Enter` to expand a plugin row and show its details:

- **Name** and **version**
- **Scope** -- `cli`, `project`, `user`, `custom path`, or the marketplace source name
- **Skills** -- names or count
- **Agents** -- names or count
- **Hooks** -- count
- **MCP servers** -- count (or `blocked` when the plugin is not trusted)
- **Description** and **path**

Use these keys in the Plugins tab:

| Key | Action |
|-----|--------|
| `r` | Reload all plugins |
| `a` | Add a plugin from `owner/repo`, a URL, or a local path |
| `Space` | Enable or disable the selected plugin |
| `x` | Uninstall the selected plugin (asks for confirmation) |
| `f` | Filter by status (all, enabled, or disabled) |
| `Enter` | Expand or collapse plugin details |
| `/` | Search plugins by name |

Uninstall asks for confirmation: press lowercase `y` to confirm, or any other key (including `Esc`) to cancel.

### Marketplace tab

Browse and install plugins from your configured marketplace sources.

Use these keys in the Marketplace tab:

| Key | Action |
|-----|--------|
| `i` | Install the selected plugin |
| `d` | Uninstall the selected plugin (asks for confirmation) |
| `a` | Add a marketplace source |
| `x` | Remove the selected source and all its plugins (asks for confirmation) |
| `r` | Refresh marketplace sources |
| `u` | Update the selected marketplace plugin |
| `Enter` | Expand or collapse a source or plugin |
| `/` | Search plugins by name |

Component summaries on list rows and per-category component details in the
expanded view appear only for marketplaces that publish a `plugin-index.json`
catalog.

---

## CLI commands

Manage plugins without starting an interactive session.

### Plugin commands

```bash
grok plugin list [--json] [--available]   # List installed plugins (--available requires --json)
grok plugin install <source> --trust      # Git URL, GitHub shorthand (user/repo), or local path
grok plugin uninstall <name> [--confirm] [--keep-data]   # Aliases: rm, remove
grok plugin update [<name>]               # Omit the name to update all plugins
grok plugin enable <name>
grok plugin disable <name>
grok plugin details <name>                # Show the plugin's component inventory
grok plugin validate [<path>]             # Validate plugin.json (default: current directory)
grok plugin tag [<path>] [--push] [--force] [--dry-run]   # Tag a release from the manifest version
```

Run `grok plugin install <source>` without `--trust` and Grok prints the source and warns that installing will activate the plugin's hooks, MCP servers, and skills, then stops without installing. Add `--trust` to install it.

The `<source>` argument accepts:

- `user/repo` -- GitHub shorthand
- `user/repo@v1.0` -- pinned to a ref
- `user/repo@<commit-sha>` -- pinned to an exact commit (verified after fetch)
- `user/repo#subdir` -- subdirectory within the repo
- `https://github.com/user/repo.git` -- full URL
- `git@github.com:user/repo.git` -- SSH
- `./local-dir` or `/absolute/path` -- local directory

### Requiring commit pins (`require_sha`)

Remote plugins are not cryptographically signed: an install that tracks a
branch or tag runs whatever that ref points at tomorrow. Operators can require
every remote install and update to pin a full commit sha (40- or 64-hex,
verified against the fetched checkout):

```toml
# config.toml
[marketplace]
require_sha = true
```

or `GROK_MARKETPLACE_REQUIRE_SHA=1`. Both are tighten-only: either one enables
the policy and neither can switch it back off. With the policy on, unpinned
remote installs, marketplace installs without a published `sha`, and updates of
branch-tracking installs are refused.

Scope: the policy covers everything fetched from a remote git URL at install or
update time. Plugins vendored inside a marketplace source itself are copied
from that source's synced checkout and are not covered — pin your marketplace
source's content by publishing `sha` entries in `plugin-index.json`.

### Marketplace commands

```bash
grok plugin marketplace list [--json]
grok plugin marketplace add <url>         # Git URL, GitHub shorthand (user/repo), or local path
grok plugin marketplace remove <url>      # Git URL or local path of a configured source
grok plugin marketplace update [<name>]   # Omit the name to refresh all sources
```

### Example: set up a team marketplace

```bash
grok plugin marketplace add my-org/team-plugins
grok plugin marketplace list
grok plugin install my-org/team-plugins --trust
grok plugin list
grok plugin update
```

---

## Slash commands

In an interactive session, these commands open the modal on a specific tab. They take no arguments — manage plugins from the modal or with the `grok plugin` CLI.

| Command | Opens |
|---------|-------|
| `/plugins` | Plugins tab |
| `/hooks` | Hooks tab |
| `/marketplace` | Marketplace tab |
| `/skills` | Skills tab |
| `/mcps` | MCP Servers tab |

---

## Configuration

Configure plugin directories and per-plugin state in `~/.grok/config.toml`:

```toml
[plugins]
paths = ["~/my-plugins/custom-tools"]        # Additional plugin directories
disabled = ["user/a1b2c3d4/noisy-plugin"]    # Plugin IDs or names to skip
enabled = ["project/9f8e7d6c/team-tools"]    # Plugin IDs or names to force on
```

List a plugin in `disabled` to discover it but skip loading its components. List a plugin in `enabled` to activate it — plugins are disabled by default unless a CLI override or an explicit config path enables them, so add them here to turn them on. Each entry is either a plain plugin name (as shown by `grok plugin list`) or a full plugin ID in the form `<scope>/<hash>/<name>`.

### Hide the plugins UI

To hide the hooks and plugins UI — the `/hooks` and `/plugins` commands and the scrollback annotations — set this in `~/.grok/pager.toml`:

```toml
disable_plugins = true
```

---

## Marketplace sources

Add git or local marketplace sources to discover and install plugins.

### In config.toml

Each source needs a `name` and either a `git` URL (with an optional `branch`) or a local `path`:

```toml
[[marketplace.sources]]
name = "My Team Plugins"
git = "https://github.com/my-org/plugins.git"

[[marketplace.sources]]
name = "Local Dev"
path = "~/dev/my-plugins"
```

### In settings.json

Add sources under `extraKnownMarketplaces`, keyed by name. Each entry's `source` is one of `git` (with `url`), `github` (with `repo`), or `local` (with `path`):

```json
{
  "extraKnownMarketplaces": {
    "my-marketplace": {
      "source": { "source": "git", "url": "git@github.com:my-org/plugins.git" }
    }
  }
}
```

Place this file at `~/.grok/settings.json` or `~/.claude/settings.json`.

---

## Trust model

Enabling a plugin loads its skills, slash commands, and agents. Trust is separate and controls whether a plugin's code runs: even for an enabled plugin, its hooks, MCP servers, and LSP servers stay inactive until you trust it. This prevents an untrusted repository from running code on your machine.

Grok trusts plugins from `~/.grok/plugins/` automatically. Project plugins in `.grok/plugins/` require explicit trust. To trust a plugin, install it with `--trust`:

```bash
grok plugin install <source> --trust
```

---

## Inspect plugins

Run `grok inspect` to see every discovered plugin and what it provides:

```bash
grok inspect          # Show plugins with their skills, agents, hooks, and MCP servers
grok inspect --json   # Emit machine-readable JSON
```

Plugin-provided components appear in their sections (Skills, Agents, MCP Servers, and so on) with a `plugin: <name>` label, so you can see where each component originates.

---

## General keyboard shortcuts

These keys work across every tab in the modal:

| Key | Action |
|-----|--------|
| `Tab` | Next tab |
| `Shift+Tab` | Previous tab |
| `j` / down-arrow | Move selection down |
| `k` / up-arrow | Move selection up |
| `Enter` | Expand or collapse the selected item |
| `/` | Search the current tab by name |
| `Esc` | Clear the search, or close the modal |

Destructive remove and uninstall actions in the modal ask for confirmation. Press lowercase `y` to confirm, or any other key (including `Esc`) to cancel.
