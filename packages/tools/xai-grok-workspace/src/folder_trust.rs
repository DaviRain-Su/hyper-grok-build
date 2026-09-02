//! Folder-trust DECISION side ("do you trust this folder?").
//!
//! This is the client/workspace half of the folder-trust gate: it scans a
//! workspace for repo-local code-exec configs, resolves the pure trust
//! [`decide`] precedence, prompts (MVP stderr), and reads/writes the durable
//! [`crate::trust::TrustStore`] (`~/.grok/trusted_folders.toml`). The
//! consume/gating half (the `DECISIONS` cache, `resolve_and_record`,
//! `project_scope_allowed`, the loader filters) lives in `xai-grok-shell`.
//!
//! ## Precedence (canonical; see [`decide`])
//! 1. Feature flag OFF  → trusted (no gating).
//! 2. Store (this workspace recorded trusted) → trusted.
//!    An explicit `--trust` grant is persisted to the store up front (see [`grant_folder_trust`]), so it is honored here.
//! 3. Key unrecordable (the user's own `$HOME`, the filesystem root, or a non-absolute path) → trusted.
//!    The store refuses to persist such an over-broad root, so gating would re-prompt forever on a key that can never persist.
//!    See [`crate::trust::is_unsafe_trust_root`].
//! 4. No repo-local code-exec configs present → trusted (nothing to gate).
//! 5. Interactive TTY   → prompt the user (y/N).
//! 6. Otherwise (headless) → untrusted.
//!
//! How the consume side caches this verdict is a `xai-grok-shell` concern, documented there.
//! (For example, the rule-4 allow is provisional and re-checked rather than cached.)

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;

use toml::Value as TomlValue;
use xai_grok_config_types::{BoolFlag, RemoteSettings};

use crate::trust::{TrustStore, workspace_key};

/// The pure trust outcome for a set of inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    /// Repo-local servers allowed.
    Trusted,
    /// Repo-local servers blocked.
    Untrusted,
    /// Interactive: ask the user.
    Prompt,
}

/// Inputs to the pure [`decide`] precedence function.
#[derive(Debug, Clone, Copy)]
pub struct DecideInputs {
    pub store_trusted: bool,
    pub repo_configs_present: bool,
    pub is_interactive: bool,
    /// False when the workspace key is an over-broad root the store refuses to record (home, filesystem root, non-absolute).
    /// See [`crate::trust::is_unsafe_trust_root`].
    pub key_recordable: bool,
}

/// Pure trust-decision precedence. No I/O; unit-tested directly.
/// See the module docs for the ordered precedence.
pub fn decide(feature_enabled: bool, i: &DecideInputs) -> TrustOutcome {
    if !feature_enabled {
        return TrustOutcome::Trusted;
    }
    if i.store_trusted {
        return TrustOutcome::Trusted;
    }
    // An over-broad root the store can't record (the user's own $HOME or fs-root, never a fetched repo) can't be durably gated
    // Trust it instead of prompting on a key that can never persist (mirrors the feature-off default)
    if !i.key_recordable {
        return TrustOutcome::Trusted;
    }
    if !i.repo_configs_present {
        return TrustOutcome::Trusted;
    }
    if i.is_interactive {
        return TrustOutcome::Prompt;
    }
    TrustOutcome::Untrusted
}

/// Gather the [`DecideInputs`] for `cwd` (store trust, repo configs, interactivity), keyed by `key`.
/// The shell's `compute` and the launch-dir resolve both gather through here, so the store read and repo-config scan cannot drift across callers.
pub fn decide_inputs(cwd: &Path, key: &Path) -> DecideInputs {
    decide_inputs_with_interactive(cwd, key, is_interactive())
}

/// Like [`decide_inputs`] but with caller-supplied interactivity, so callers that determine it differently still share the same gather.
/// The pager TUI passes `stdin().is_terminal()` ONLY.
/// It redirects native stderr before resolving trust, so the default [`is_interactive`] (`stdin && stderr`) is false and the prompt never shows.
pub fn decide_inputs_with_interactive(
    cwd: &Path,
    key: &Path,
    is_interactive: bool,
) -> DecideInputs {
    DecideInputs {
        store_trusted: is_trusted_this_process(key),
        // Deliberate second discover: the caller's `key` came from `workspace_key`, its own git2 discover
        // `repo_configs_present` runs `RepoDirChain::resolve`, which discovers the same repo again
        // Collapsing the two would mean threading the resolved root into key derivation, rippling `workspace_key` repo-wide
        repo_configs_present: repo_configs_present(cwd),
        is_interactive,
        // An over-broad key (home / fs-root / non-absolute) can never be recorded
        // by the store, so decide() trusts it rather than prompt on a key that
        // can't persist (Case 2: cwd IS $HOME, incl. the default `~/.grok`).
        key_recordable: !crate::trust::is_unsafe_trust_root(key),
    }
}

/// Whether the whole folder-trust system is inert (auto-trusts everything) for this binary: true on a local/dev build (no `GROK_VERSION` stamp).
/// Every trust auto-grant site calls this; when true grok never prompts, never gates repo-local configs, and does no `trusted_folders.toml` I/O.
pub fn folder_trust_inert() -> bool {
    is_local_build()
}

/// Whether this binary was built without a release version stamp (`GROK_VERSION` unset at compile time), i.e. a local/dev build.
/// Kept local rather than in `xai-grok-version`: adding a symbol to that near-universal crate widens the rebuild/test fan-out for unrelated targets.
/// `option_env!` resolves the same in any crate. Cross-crate callers use [`folder_trust_inert`].
fn is_local_build() -> bool {
    // Runtime escape hatch: a pinned GROK_TEST_VERSION simulates a release build
    // Tests/CI run unstamped, so they look like local builds; this lets them exercise the gate
    if std::env::var(xai_grok_version::TEST_VERSION_ENV).is_ok() {
        return false;
    }
    option_env!("GROK_VERSION").is_none()
}

/// Resolve whether the folder-trust gate is enabled.
///
/// On a local/dev build (no `GROK_VERSION` release stamp) the feature is OFF regardless of env/config/remote: a self-built grok auto-trusts.
/// Folder-trust applies only to shipped, release-stamped binaries.
///
/// On a release-stamped build, normal precedence (via `BoolFlag`):
/// env `GROK_FOLDER_TRUST` > `[folder_trust] enabled` (user) > managed > remote `folder_trust_enabled` > default **true**.
/// The remote kill-switch or a `[folder_trust] enabled = false` opt-out turns it back off.
pub fn feature_enabled(remote: Option<&RemoteSettings>) -> bool {
    feature_enabled_for_build(remote, is_local_build())
}

/// `feature_enabled` with the local-build flag fed in so both arms are unit-testable.
fn feature_enabled_for_build(remote: Option<&RemoteSettings>, is_local_build: bool) -> bool {
    // Local/dev builds never gate (auto-trust): folder-trust applies only to shipped, release-stamped binaries
    // Even an explicit GROK_FOLDER_TRUST/config opt-in is ignored here so a self-built grok never prompts
    if is_local_build {
        return false;
    }
    fn from_toml(v: Option<&TomlValue>) -> Option<bool> {
        v?.get("folder_trust")?.get("enabled")?.as_bool()
    }
    let user = xai_grok_config::load_from_disk().ok();
    let managed = xai_grok_config::load_managed_config().ok();
    BoolFlag::env("GROK_FOLDER_TRUST")
        .config(from_toml(user.as_ref()))
        .managed(from_toml(managed.as_ref()))
        .feature_flag(remote.and_then(|r| r.folder_trust_enabled))
        .default(true)
        .resolve()
        .value
}

/// Process-local explicit grant/deny, separate from [`TrustStore`] durability.
/// When the sandbox denies the store write, this process still honors the latest user decision.
/// The consume side reads it via [`is_trusted_this_process`].
static PROCESS_DECISIONS: LazyLock<Mutex<HashMap<PathBuf, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn record_process_decision(key: &Path, trusted: bool) {
    PROCESS_DECISIONS.lock().insert(key.to_path_buf(), trusted);
}

/// Latest process-local decision, else the durable store.
pub fn is_trusted_this_process(key: &Path) -> bool {
    if let Some(&trusted) = PROCESS_DECISIONS.lock().get(key) {
        return trusted;
    }
    TrustStore::load().is_trusted(key)
}

/// Persist an explicit `--trust` grant.
/// Best-effort on disk; a process-local grant is always recorded so this session honors the user decision.
pub fn grant_folder_trust(cwd: &Path) {
    // Local/dev builds never gate, so there is nothing to grant: `--trust` is a no-op and the store is left untouched (the whole feature is inert)
    if folder_trust_inert() {
        return;
    }
    let key = workspace_key(cwd);
    if crate::trust::is_unsafe_trust_root(&key) {
        return;
    }
    let mut store = TrustStore::load();
    if store.has_decision(&key) && store.is_trusted(&key) {
        record_process_decision(&key, true);
        return;
    }
    persist_trust(&mut store, &key);
}

/// Revoke trust for `cwd`'s workspace in the durable store and this process.
///
/// A never-trusted folder stays undecided: do not persist a decision the user did not make.
/// Symmetric with [`grant_folder_trust`].
pub fn revoke_folder_trust_store(cwd: &Path) -> bool {
    // Local/dev builds never wrote the store, so there is nothing to revoke.
    if folder_trust_inert() {
        return false;
    }
    let key = workspace_key(cwd);
    let mut store = TrustStore::load();
    let was_trusted =
        store.is_trusted(&key) || PROCESS_DECISIONS.lock().get(&key).copied() == Some(true);
    if was_trusted {
        if let Err(e) = store.set_untrusted(&key) {
            tracing::warn!(
                path = %key.display(),
                error = %e,
                "folder trust: failed to persist untrust decision"
            );
        }
        record_process_decision(&key, false);
    }
    was_trusted
}

pub fn persist_trust(store: &mut TrustStore, key: &Path) {
    if let Err(e) = store.set_trusted(key) {
        tracing::warn!(
            path = %key.display(),
            error = %e,
            "folder trust: failed to persist trust decision"
        );
    }
    record_process_decision(key, true);
}

/// Whether any repo-local trust-sensitive config is present for `cwd`.
/// When none are present there is nothing to gate, so we skip the prompt entirely.
/// Thin wrapper over [`collect_repo_config_kinds`] with `first_only = true`, so this hot path short-circuits on the first hit.
/// The gate and the display-only [`repo_config_kinds`] therefore enumerate the EXACT same markers and cannot drift.
pub fn repo_configs_present(cwd: &Path) -> bool {
    !collect_repo_config_kinds(cwd, true).is_empty()
}

/// Display-only (not itself the trust gate): the repo-local trust-sensitive config kinds present for `cwd`, deduped in cheap-to-expensive order.
/// Kinds: `mcp`, `plugins`, `permission`, `lsp`, `envrc`, `claude`, `hooks`, `agents`, `roles`, `personas`, `workflows`.
/// Single source with [`repo_configs_present`], which is `!repo_config_kinds(cwd).is_empty()`.
/// A folder the gate fired on therefore always has a non-empty, accurate kind list.
pub fn repo_config_kinds(cwd: &Path) -> Vec<&'static str> {
    collect_repo_config_kinds(cwd, false)
}

/// Whether a project `.grok/config.toml` `[permission]` value would contribute rules to the permission resolver.
/// Mirrors the shapes `permission::resolution` loads: non-empty `allow`/`deny`/`ask` arrays, or a non-empty verbose `rules` array.
/// Empty arrays and empty tables do not gate (same as an empty `[mcp_servers]` or `[plugins].paths`).
fn config_toml_permission_contributes(permission_value: &TomlValue) -> bool {
    let Some(table) = permission_value.as_table() else {
        // Non-table `[permission]` fails config load elsewhere
        // Treat it as a marker so a malicious non-table still trips the gate rather than resolving trusted
        return true;
    };
    for key in ["deny", "allow", "ask"] {
        if table
            .get(key)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
        {
            return true;
        }
    }
    table
        .get("rules")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
}

fn path_present_or_uncertain(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn directory_present_or_uncertain(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Shared scanner behind [`repo_configs_present`] and [`repo_config_kinds`].
/// With `first_only` it returns immediately after the first marker (the gate's short-circuit); otherwise it collects every distinct kind.
fn collect_repo_config_kinds(cwd: &Path, first_only: bool) -> Vec<&'static str> {
    // Resolve the git root and the cwd-to-root dir chain ONCE and reuse them across the git2-based marker checks below
    // This gate then does one git2 discover and one git2 walk instead of one discover and walk per marker check
    // On a non-git dir each discover walks to the filesystem root, and Windows taxes every such syscall 10-100x
    // The `.claude` settings-compat check keeps its own cheap `.git`-existence walk on purpose; see that check
    // Checks run cheap to expensive and short-circuit on the first hit when `first_only`
    let chain = xai_grok_agent::repo::RepoDirChain::resolve(cwd);
    let mut kinds: Vec<&'static str> = Vec::new();
    // Record a distinct kind; when `first_only`, return as soon as one is found
    macro_rules! hit {
        ($k:expr) => {{
            let k: &'static str = $k;
            if !kinds.contains(&k) {
                kinds.push(k);
            }
            if first_only {
                return kinds;
            }
        }};
    }

    // `.mcp.json` anywhere from repo root down to cwd.
    if !crate::project_config::find_mcp_json_files_in(&chain.dirs).is_empty() {
        hit!("mcp");
    }
    // Project `.grok/config.toml` markers: a non-empty `[mcp_servers]` table or `[plugins].paths` array, or a contributing `[permission]` section
    // `[plugins].paths` loads as auto-trusted ConfigPath plugins; `[permission]` allow/deny/ask rules auto-approve or block tools
    // A clone whose ONLY repo-local config is either must still be gated (else it resolves Trusted and the loader runs ungated)
    for path in crate::project_config::find_project_configs_in(&chain.dirs) {
        let Ok(root) = xai_grok_config::load_config_file(&path) else {
            continue;
        };
        let has_mcp_servers = root
            .get("mcp_servers")
            .and_then(|v| v.as_table())
            .is_some_and(|t| !t.is_empty());
        let has_plugin_paths = root
            .get("plugins")
            .and_then(|v| v.get("paths"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let has_permission = root
            .get("permission")
            .is_some_and(config_toml_permission_contributes);
        if has_mcp_servers {
            hit!("mcp");
        }
        if has_plugin_paths {
            hit!("plugins");
        }
        if has_permission {
            hit!("permission");
        }
    }
    // Project `.grok/lsp.json`.
    if cwd.join(".grok").join("lsp.json").is_file() {
        hit!("lsp");
    }
    // Project `.cursor/mcp.json`: vendor MCP loading is default-on and tagged `Project`, so a repo shipping ONLY this file must still be gated
    // File presence is enough
    if cwd.join(".cursor").join("mcp.json").is_file() {
        hit!("mcp");
    }
    // Project `.envrc` is auto-sourced in a bash subshell when `direnv` isn't installed (direct code-exec), so an `.envrc`-only clone must be gated
    // The loader reads `<cwd>/.envrc` directly (NOT a git-root walk), so probe at cwd to match exactly what gets executed
    if cwd.join(".envrc").is_file() {
        hit!("envrc");
    }
    // Hook loading reads project `.claude/settings.json` and `settings.local.json` at the git root only
    // The ENV and permission loaders walk EVERY dir from cwd to the repo root (`collect_project_claude_paths`)
    // Detect along the SAME walk via the shared reader, else a `.claude` `env` in a subdir (injected into every spawned subprocess) loads ungated
    // Keeps its own `.git`-existence walk (NOT the git2 chain) so detection stays identical to the loader, which bounds on a bare/empty `.git` too
    if crate::permission::claude_settings::project_claude_settings_present(cwd) {
        hit!("claude");
    }
    // Other project HOOK sources are resolved from the git worktree root only (the chain's `git_root`), NOT cwd
    // Hook discovery resolves from the same root via `workspace_key`, so root-level hooks are gated even when launched from a subdir
    // A repo-local hook file/dir is repo-controlled code-exec that must be gated
    // Otherwise a hooks-only clone (e.g. `.grok/hooks/evil.json`) would resolve trusted and run ungated.
    // Presence mirrors discovery's "something to gate" check
    let hook_root = chain.git_root.as_deref().unwrap_or(cwd);
    if path_present_or_uncertain(&hook_root.join(".grok").join("hooks"))
        || hook_root.join(".cursor").join("hooks.json").is_file()
    {
        hit!("hooks");
    }
    // Project PLUGIN dirs: project-scoped plugins fall under folder-trust too, so a repo-local plugin dir is repo-controlled code-exec (hooks/MCP)
    // Else a plugin clone (e.g. `.grok/plugins/evil/`, even one in a subdir launched via `cd sub && grok`) would resolve trusted and run ungated.
    // Uses the shared cwd-to-git-root walk so detection matches exactly what `discover_plugins` scans for Project scope, erring on the secure side
    if !xai_grok_agent::plugins::project_plugin_dirs_in(&chain.dirs).is_empty() {
        hit!("plugins");
    }
    // Project AGENT dirs (`.grok/agents` / `.claude/agents`): an agents-only clone must still be gated
    // A project agent definition can carry an inline `hooks:` block (repo-controlled code-exec) and can shadow a built-in subagent by name
    // Uses the shared cwd-to-git-root walk so detection can't drift from agent discovery (same pattern as the plugin check above)
    if !xai_grok_agent::discovery::project_agent_dirs_in(&chain.dirs).is_empty() {
        hit!("agents");
    }
    // Presence matches exact-cwd discovery without parsing repository content.
    let grok = cwd.join(".grok");
    if directory_present_or_uncertain(&grok.join("roles")) {
        hit!("roles");
    }
    if directory_present_or_uncertain(&grok.join("personas")) {
        hit!("personas");
    }
    if directory_present_or_uncertain(&hook_root.join(".grok").join("workflows")) {
        hit!("workflows");
    }
    // `~/.claude.json` `projects.<cwd>.mcpServers`.
    if claude_project_mcp_present(cwd) {
        hit!("mcp");
    }
    kinds
}

/// Display names under `~/.claude.json projects.<cwd>.mcpServers`, or `None` when the file/entry is absent or the object is empty.
/// Both [`claude_project_mcp_present`] and the shell's `project_scoped_mcp_names` derive from this one reader, so they never drift.
pub fn claude_project_mcp_names(cwd: &Path) -> Option<Vec<String>> {
    let home = xai_dirs::home_dir()?;
    let content = std::fs::read_to_string(home.join(".claude.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let cwd_key = cwd.to_string_lossy();
    let names: Vec<String> = value
        .get("projects")
        .and_then(|p| p.get(cwd_key.as_ref()))
        .and_then(|proj| proj.get("mcpServers"))
        .and_then(|m| m.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    (!names.is_empty()).then_some(names)
}

fn claude_project_mcp_present(cwd: &Path) -> bool {
    claude_project_mcp_names(cwd).is_some()
}

fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// MVP trust prompt: a plain stderr warning and a stdin y/N read.
/// Defaults to NO on empty input, EOF, or any non-yes answer.
/// Deliberately minimal (no ACP modal).
pub fn prompt_for_trust(key: &Path) -> bool {
    use std::io::{BufRead, Write};

    let mut err = std::io::stderr();
    let _ = writeln!(err);
    let _ = writeln!(
        err,
        "This folder contains repo-local config (.mcp.json / .grok/lsp.json / hooks) \
         that can run commands on your machine."
    );
    let _ = writeln!(err, "  Folder: {}", key.display());
    let _ = write!(
        err,
        "Trust the authors of this folder and allow these servers to start? [y/N] "
    );
    let _ = err.flush();

    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> DecideInputs {
        DecideInputs {
            store_trusted: false,
            repo_configs_present: true,
            is_interactive: false,
            // Default: a normal (recordable) key, so the Case-2 rule doesn't fire and every `..inputs()` spread exercises the other rules
            key_recordable: true,
        }
    }

    #[test]
    fn feature_off_is_always_trusted() {
        // Even with everything pointing to untrusted, feature off resolves trusted
        assert_eq!(decide(false, &inputs()), TrustOutcome::Trusted);
    }

    #[test]
    fn store_trusted_is_trusted() {
        let i = DecideInputs {
            store_trusted: true,
            ..inputs()
        };
        assert_eq!(decide(true, &i), TrustOutcome::Trusted);
    }

    #[test]
    fn no_repo_configs_is_trusted_without_prompt() {
        let i = DecideInputs {
            repo_configs_present: false,
            is_interactive: true,
            ..inputs()
        };
        // Nothing to gate resolves Trusted, never Prompt
        assert_eq!(decide(true, &i), TrustOutcome::Trusted);
    }

    #[test]
    fn interactive_with_configs_prompts() {
        let i = DecideInputs {
            is_interactive: true,
            ..inputs()
        };
        assert_eq!(decide(true, &i), TrustOutcome::Prompt);
    }

    #[test]
    fn headless_with_configs_is_untrusted() {
        assert_eq!(decide(true, &inputs()), TrustOutcome::Untrusted);
    }

    #[test]
    fn unrecordable_key_is_trusted_even_with_configs_and_interactive() {
        // Case 2: cwd == $HOME (or fs-root / non-absolute)
        // The store can't record such a key, so gating would re-prompt forever; decide() trusts it, ahead of the repo-configs and interactive rules
        let i = DecideInputs {
            store_trusted: false,
            repo_configs_present: true,
            is_interactive: true,
            key_recordable: false,
        };
        assert_eq!(decide(true, &i), TrustOutcome::Trusted);
    }

    /// A `git init`'d temp dir, so repo discovery is bounded to it instead of any ancestor repo the system temp dir lives in.
    /// `find_mcp_json_files` / `find_project_configs` discover the enclosing repo and walk to its root.
    fn repo_tmp() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn repo_configs_present_false_when_empty() {
        let tmp = repo_tmp();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_mcp_json() {
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".mcp.json"), "{}").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_grok_config_mcp_servers() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("config.toml"), "[mcp_servers.x]\ncommand=\"y\"\n").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_grok_lsp_json() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("lsp.json"), "{}").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_cursor_mcp_json() {
        let tmp = repo_tmp();
        let cursor = tmp.path().join(".cursor");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(cursor.join("mcp.json"), "{}").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_envrc() {
        // An `.envrc`-only clone is auto-sourced in a bash subshell (direct RCE)
        // It must resolve untrusted even though it has no MCP/LSP/hook configs
        let tmp = repo_tmp();
        std::fs::write(tmp.path().join(".envrc"), "export FOO=bar\n").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_project_agents() {
        // A `.grok/agents`-only clone must be gated
        // A project agent definition can carry an inline `hooks:` block (code-exec) and can shadow a built-in subagent by name
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("agents")).unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_claude_agents() {
        // `.claude/agents` is the vendor-compat project agent dir; same gate.
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".claude").join("agents")).unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_project_agents_from_subdir() {
        // Agents live at the git root but the session is launched from a subdir
        // Detection walks from cwd to the git root exactly like agent discovery, so it must still fire (a cwd-only probe would miss it)
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("agents")).unwrap();
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(repo_configs_present(&subdir));
    }

    #[test]
    fn repo_configs_present_detects_project_roles() {
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("roles")).unwrap();

        assert!(repo_configs_present(tmp.path()));
        assert!(repo_config_kinds(tmp.path()).contains(&"roles"));
    }

    #[test]
    fn repo_configs_present_detects_project_personas() {
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("personas")).unwrap();

        assert!(repo_configs_present(tmp.path()));
        assert!(repo_config_kinds(tmp.path()).contains(&"personas"));
    }

    #[test]
    fn project_subagent_marker_regular_file_is_absent() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("roles"), "not a directory").unwrap();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn project_subagent_marker_at_repo_root_is_absent_from_subdir() {
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok/roles")).unwrap();
        let subdir = tmp.path().join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(!repo_configs_present(&subdir));
    }

    #[cfg(unix)]
    #[test]
    fn project_subagent_marker_symlink_to_directory_is_present() {
        let tmp = repo_tmp();
        let target = tmp.path().join("target-roles");
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&grok).unwrap();
        std::os::unix::fs::symlink(&target, grok.join("roles")).unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_project_subagent_marker_is_absent() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::os::unix::fs::symlink("missing", grok.join("personas")).unwrap();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_project_workflows_from_subdir() {
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("workflows")).unwrap();
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(repo_configs_present(&subdir));
        assert!(repo_config_kinds(&subdir).contains(&"workflows"));
    }

    #[test]
    fn repo_configs_present_detects_claude_settings_from_subdir() {
        // A `.claude/settings.json` `env` in a SUBDIR (no other repo config), launched from that subdir, must be detected
        // The env loader walks from cwd to the repo root, so detection walks the same path (a git-root-only probe would miss it)
        let tmp = repo_tmp();
        let subdir = tmp.path().join("crates").join("inner");
        let claude = subdir.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"env":{"X":"1"}}"#).unwrap();
        assert!(repo_configs_present(&subdir));
    }

    #[test]
    fn repo_configs_present_detects_project_hooks() {
        // A hooks-only repo (no MCP/LSP configs) must still be gated, so its project hooks don't run ungated when the folder is untrusted
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_project_hooks_file() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("hooks"), "{}").unwrap();

        assert!(repo_configs_present(tmp.path()));
        assert!(repo_config_kinds(tmp.path()).contains(&"hooks"));
    }

    #[cfg(unix)]
    #[test]
    fn repo_configs_present_detects_dangling_project_hooks_symlink() {
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::os::unix::fs::symlink("missing-hooks", grok.join("hooks")).unwrap();

        assert!(repo_configs_present(tmp.path()));
        assert!(repo_config_kinds(tmp.path()).contains(&"hooks"));
    }

    #[test]
    fn repo_configs_present_detects_project_hooks_from_subdir() {
        // Hooks live at the git root but the session is launched from a subdir
        // The gate must still fire because discovery resolves hooks from the root
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("hooks")).unwrap();
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(repo_configs_present(&subdir));
    }

    #[test]
    fn repo_configs_present_detects_project_plugins() {
        // A plugin-only repo (no MCP/LSP/hooks configs) must still be gated
        // Otherwise a project plugin's hooks/MCP would run ungated when the folder is untrusted
        let tmp = repo_tmp();
        std::fs::create_dir_all(tmp.path().join(".grok").join("plugins").join("x")).unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_project_plugins_in_subdir() {
        // A plugin under a subdir (root otherwise clean), launched from that subdir, must still be gated
        // Detection walks from cwd to the git root exactly like discover_plugins, so a subdir-only plugin is not a fail-open hole
        let tmp = repo_tmp();
        let subdir = tmp.path().join("packages").join("foo");
        std::fs::create_dir_all(subdir.join(".grok").join("plugins").join("evil")).unwrap();
        assert!(repo_configs_present(&subdir));
    }

    #[test]
    fn repo_configs_present_false_for_empty_mcp_servers_table() {
        // A project config whose `[mcp_servers]` table is empty has nothing to gate, so it must not trip the gate
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("config.toml"), "[mcp_servers]\n").unwrap();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_grok_config_plugins_paths() {
        // A repo whose ONLY repo-local config is `[plugins].paths` (no plugin dir, no MCP/LSP/hooks) must still be gated
        // Those paths load as auto-trusted ConfigPath plugins, so an ungated clone is a live RCE
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("config.toml"), "[plugins]\npaths = [\"./x\"]\n").unwrap();
        assert!(repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_false_for_empty_plugins_paths() {
        // An empty `[plugins].paths` (or a `[plugins]` table without `paths`) contributes no plugin code-exec, so it must not trip the gate
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(grok.join("config.toml"), "[plugins]\npaths = []\n").unwrap();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_configs_present_detects_grok_config_permission() {
        // A repo whose ONLY repo-local config is a contributing `[permission]` section (no MCP/plugins/hooks) must still be gated
        // Those allow rules auto-approve tool calls, so an ungated clone loads the attacker's policy
        // Also covers subdir launch (the cwd-to-git-root walk)
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("config.toml"),
            "[permission]\nallow = [\"Bash(*)\"]\n",
        )
        .unwrap();
        assert!(repo_configs_present(tmp.path()));
        assert!(
            repo_config_kinds(tmp.path()).contains(&"permission"),
            "permission-only repo must report the permission kind"
        );
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(
            repo_configs_present(&subdir),
            "permission-only config at git root must gate subdir launches"
        );
    }

    #[test]
    fn repo_configs_present_false_for_empty_permission() {
        // Empty allow/deny/ask arrays contribute no rules, so they must not trip the gate (mirrors empty `[mcp_servers]` / empty `[plugins].paths`)
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("config.toml"),
            "[permission]\nallow = []\ndeny = []\n",
        )
        .unwrap();
        assert!(!repo_configs_present(tmp.path()));
    }

    #[test]
    fn repo_config_kinds_matches_gate_and_reports_all_kinds() {
        // Single-source guard: `repo_config_kinds` must agree with the gate (`repo_configs_present == !repo_config_kinds(..).is_empty()`)
        // It must also report `plugins` via `[plugins].paths`, `claude` via `.claude/settings.json`, and `agents` via `.grok/agents`
        // Even a SUBDIR launch must report them (the cwd-to-git-root walk that `first_only` shares)
        // Guards against silent drift between the two
        let tmp = repo_tmp();
        let grok = tmp.path().join(".grok");
        std::fs::create_dir_all(grok.join("agents")).unwrap();
        std::fs::write(grok.join("config.toml"), "[plugins]\npaths = [\"./x\"]\n").unwrap();
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"env":{"X":"1"}}"#).unwrap();
        // Launch from a subdir: the walk must still find the root-level markers.
        let subdir = tmp.path().join("crates").join("inner");
        std::fs::create_dir_all(&subdir).unwrap();

        let kinds = repo_config_kinds(&subdir);
        for expected in ["plugins", "claude", "agents"] {
            assert!(
                kinds.contains(&expected),
                "repo_config_kinds missing {expected:?} (subdir launch); got {kinds:?}"
            );
        }
        // Gate and kinds must agree: a configured repo and an empty one
        assert_eq!(
            repo_configs_present(&subdir),
            !repo_config_kinds(&subdir).is_empty(),
            "gate must equal !kinds.is_empty() for a configured repo"
        );
        let empty = repo_tmp();
        assert!(!repo_configs_present(empty.path()));
        assert!(repo_config_kinds(empty.path()).is_empty());
        assert_eq!(
            repo_configs_present(empty.path()),
            !repo_config_kinds(empty.path()).is_empty(),
            "gate must equal !kinds.is_empty() for an empty repo"
        );
    }

    // GROK_HOME isolation mirrored from this crate's `permission::claude_compat` tests
    // The workspace crate has no `serial_test` or `xai-grok-test-support` dev-dep
    // nextest runs each test in its own process; `ENV_LOCK` serializes the rare in-process `cargo test` thread
    // `EnvVarGuard` restores the prior value on drop so a panic can't leak state
    // The crate-shared lock also serializes against other env-mutating test modules (e.g. `trust`, `worktree`) under `cargo test --lib`.
    use crate::ENV_TEST_LOCK as ENV_LOCK;

    // The crate-shared env-var guard (one definition in `lib.rs`), aliased to the local `EnvVarGuard` name
    use crate::TestEnvGuard as EnvVarGuard;

    /// Simulate a release-stamped build so store I/O runs (a local/dev build makes grant/revoke no-ops).
    /// Hold the returned guard for the test body.
    fn simulate_release_build() -> EnvVarGuard {
        EnvVarGuard::set(xai_grok_version::TEST_VERSION_ENV, Path::new("0.0.0-sim"))
    }

    #[test]
    fn local_build_ignores_remote_rollout() {
        // A local/dev build never gates (auto-trust): even a remote rollout enable is ignored
        // The feature stays off and resolves Trusted with repo configs present and interactive
        // (Env/config isolated to unset so the remote flag is unambiguously the only enable being dropped here.)
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        let _flag = EnvVarGuard::unset("GROK_FOLDER_TRUST");

        let remote = RemoteSettings {
            folder_trust_enabled: Some(true),
            ..Default::default()
        };
        let feature = feature_enabled_for_build(Some(&remote), true);
        assert!(!feature);
        let i = DecideInputs {
            is_interactive: true,
            ..inputs()
        };
        assert_eq!(decide(feature, &i), TrustOutcome::Trusted);
    }

    #[test]
    fn release_build_keeps_gate_when_enabled() {
        // A release-stamped build (is_local_build=false) honors the remote enable
        // Isolate config so neither on-disk user/managed config nor an ambient env flag can override it
        // That means an empty GROK_HOME (no config.toml/managed_config.toml) and GROK_FOLDER_TRUST unset
        // nextest's process-per-test makes grok_home()'s OnceLock pick up the temp dir
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        let _flag = EnvVarGuard::unset("GROK_FOLDER_TRUST");

        let remote = RemoteSettings {
            folder_trust_enabled: Some(true),
            ..Default::default()
        };
        let feature = feature_enabled_for_build(Some(&remote), false);
        assert!(feature);
        let i = DecideInputs {
            is_interactive: true,
            ..inputs()
        };
        assert_eq!(decide(feature, &i), TrustOutcome::Prompt);
    }

    #[test]
    fn local_build_ignores_explicit_env_optin() {
        // Auto-trust is absolute on a local build: even an explicit GROK_FOLDER_TRUST=1 does NOT enable the feature
        // A self-built grok therefore never prompts
        // GROK_HOME is isolated so on-disk config can't influence it
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        let _flag = EnvVarGuard::set("GROK_FOLDER_TRUST", Path::new("1"));

        assert!(!feature_enabled_for_build(None, true));
    }

    #[test]
    fn release_build_defaults_on() {
        // A release-stamped build with no env/config/managed/remote signal defaults the feature ON
        // An empty GROK_HOME (no config.toml/managed config) and GROK_FOLDER_TRUST unset leave only the default
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        let _flag = EnvVarGuard::unset("GROK_FOLDER_TRUST");

        assert!(feature_enabled_for_build(None, false));
    }

    #[test]
    fn is_local_build_honors_test_version_override() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A pinned GROK_TEST_VERSION simulates a release build, so it is not a local build
        {
            let _sim = EnvVarGuard::set(xai_grok_version::TEST_VERSION_ENV, Path::new("0.0.0-sim"));
            assert!(!is_local_build());
        }
        // With it unset, an unstamped build (no GROK_VERSION) is a local build.
        // Guard to the unstamped case so a release-stamped test binary (CI release) doesn't spuriously fail this arm
        let _unset = EnvVarGuard::unset(xai_grok_version::TEST_VERSION_ENV);
        if option_env!("GROK_VERSION").is_none() {
            assert!(is_local_build());
        }
    }

    #[test]
    fn store_io_is_noop_on_local_build() {
        // On a local/dev build the whole feature is inert
        // Both halves pin a guard via a UNIQUE per-repo key (never store-file existence) so they hold under single-process `cargo test` too
        // Assert ONLY when compiled unstamped (mirrors `is_local_build_honors_test_version_override`)
        // GROK_HOME is isolated and ENV_LOCK held so toggling GROK_TEST_VERSION is race-safe
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        let _unset = EnvVarGuard::unset(xai_grok_version::TEST_VERSION_ENV);
        if option_env!("GROK_VERSION").is_some() {
            return; // a release-stamped test binary is not a local build
        }
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());

        // grant is a no-op: a local-build grant never trusts the fresh key.
        grant_folder_trust(tmp.path());
        assert!(
            !TrustStore::load().is_trusted(&key),
            "local build: grant_folder_trust must not trust the folder"
        );

        // Seed a genuinely-trusted folder under a simulated release build (so the store actually records the grant)
        // The guard drops at block end, so the build looks local again
        {
            let _sim = simulate_release_build();
            let mut store = TrustStore::load();
            store.set_trusted(&key).unwrap();
            assert!(
                TrustStore::load().is_trusted(&key),
                "release build: seeding must record the trust grant"
            );
        }

        // revoke is a no-op: a local-build revoke returns false AND leaves the grant intact
        // (Without the guard it would `set_untrusted` and return true.)
        assert!(
            !revoke_folder_trust_store(tmp.path()),
            "local build: revoke_folder_trust_store must return false"
        );
        assert!(
            TrustStore::load().is_trusted(&key),
            "local build: revoke_folder_trust_store must not untrust the folder"
        );
    }

    #[test]
    fn revoke_folder_trust_store_persists_untrust_for_trusted_folder() {
        // This tests the store half of revoke directly (not just via the shell wrapper)
        // A previously-trusted folder reports was_trusted=true AND gets an explicit `set_untrusted` persisted, so it is untrusted on reload
        // GROK_HOME is isolated so the seed/deny hit a temp store, not the real file
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set("GROK_HOME", home.path());
        let _sim = simulate_release_build();
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());

        let mut store = TrustStore::load();
        store.set_trusted(&key).unwrap();
        assert!(TrustStore::load().is_trusted(&key));

        assert!(
            revoke_folder_trust_store(tmp.path()),
            "a trusted folder must report was_trusted=true"
        );
        assert!(
            !TrustStore::load().is_trusted(&key),
            "store-only revoke must persist set_untrusted for a trusted folder"
        );
    }

    #[test]
    fn revoke_folder_trust_store_writes_no_deny_for_never_trusted_folder() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set("GROK_HOME", home.path());
        let _sim = simulate_release_build();
        let tmp = repo_tmp();

        assert!(
            !revoke_folder_trust_store(tmp.path()),
            "revoking a never-trusted folder must return false"
        );

        let store = TrustStore::load();
        assert!(
            !store.has_decision(&workspace_key(tmp.path())),
            "revoking a never-trusted folder must not record a child deny"
        );
    }

    #[test]
    fn grant_folder_trust_skips_rewrite_when_already_trusted_but_flips_untrust() {
        // Already-trusted grant must not rewrite the store; an explicit untrust record must still persist `--trust`
        // GROK_HOME is isolated so the seed hits a temp store, not the real file
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set("GROK_HOME", home.path());
        let _sim = simulate_release_build();
        let tmp = repo_tmp();
        let key = workspace_key(tmp.path());

        grant_folder_trust(tmp.path());
        assert!(
            TrustStore::load().is_trusted(&key),
            "first grant must persist trust"
        );

        let store_path = TrustStore::default_path().expect("isolated GROK_HOME");
        let after_grant = std::fs::read(&store_path).unwrap();
        // Marker a rewrite would drop.
        let mut marked = after_grant.clone();
        marked.extend_from_slice(b"\n# keep-me\n");
        std::fs::write(&store_path, &marked).unwrap();

        grant_folder_trust(tmp.path());
        let after_skip = std::fs::read(&store_path).unwrap();
        assert!(
            after_skip
                .windows(b"# keep-me".len())
                .any(|w| w == b"# keep-me"),
            "already-trusted grant must not rewrite the store"
        );
        assert!(TrustStore::load().is_trusted(&key));

        let mut store = TrustStore::load();
        store.set_untrusted(&key).unwrap();
        assert!(store.has_decision(&key));
        assert!(!store.is_trusted(&key));

        grant_folder_trust(tmp.path());
        assert!(
            TrustStore::load().is_trusted(&key),
            "explicit untrust record must still persist --trust"
        );
    }

    #[test]
    fn grant_folder_trust_records_process_local_grant_when_persist_denied() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sim = simulate_release_build();
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("GROK_HOME", home.path());
        // TrustStore writes through user_grok_home(), which reads the grok_home() OnceLock
        // Bazel rust_test is one process, so the test denies persistence at the cached home
        let store_home = xai_grok_config::user_grok_home().expect("GROK_HOME is set");
        let deny_path = store_home.join(xai_grok_config::TRUSTED_FOLDERS_FILENAME);
        if deny_path.is_file() {
            std::fs::remove_file(&deny_path).unwrap();
        }
        std::fs::create_dir_all(&deny_path).unwrap();
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _restore = Restore(deny_path);
        let tmp = repo_tmp();
        grant_folder_trust(tmp.path());
        let key = workspace_key(tmp.path());
        assert!(
            !TrustStore::load().is_trusted(&key),
            "durable store must stay ungranted when persist is denied"
        );
        assert!(
            is_trusted_this_process(&key),
            "explicit grant must be visible in-process after persist failure"
        );

        assert!(
            revoke_folder_trust_store(tmp.path()),
            "untrust must see the process-local grant"
        );
        assert!(
            !is_trusted_this_process(&key),
            "untrust must override the process-local grant even when persist is denied"
        );
        assert!(
            !decide_inputs(tmp.path(), &key).store_trusted,
            "reload/consume must not re-allow the revoked process-local grant"
        );
    }

    #[test]
    fn decide_inputs_flags_home_key_unrecordable() {
        // Case-2 wiring: cwd == $HOME, git-init'd so workspace_key discovers it as the home git root
        // The gather flags key_recordable=false and decide() trusts it despite configs and interactive
        // Pin HOME and USERPROFILE so xai_dirs::home_dir() sees the tempdir on Windows
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", home.path());
        let _userprofile = EnvVarGuard::set("USERPROFILE", home.path());
        git2::Repository::init(home.path()).unwrap();

        let home_key = crate::trust::workspace_key(home.path());
        let home_inputs = decide_inputs_with_interactive(home.path(), &home_key, true);
        assert!(
            !home_inputs.key_recordable,
            "cwd == $HOME must gather key_recordable=false"
        );
        assert_eq!(
            decide(true, &home_inputs),
            TrustOutcome::Trusted,
            "an unrecordable home key resolves Trusted (no prompt, no gate)"
        );

        // A non-home repo subdir key is recordable; the Case-2 rule can't over-trigger for a real fetched repo
        let repo = repo_tmp();
        let subdir = repo.path().join("pkg");
        std::fs::create_dir_all(&subdir).unwrap();
        let repo_key = crate::trust::workspace_key(&subdir);
        let repo_inputs = decide_inputs_with_interactive(&subdir, &repo_key, true);
        assert!(
            repo_inputs.key_recordable,
            "a non-home repo key must gather key_recordable=true"
        );
    }
}
