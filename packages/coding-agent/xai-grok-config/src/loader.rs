//! TOML loading, layered merging, and `$VAR` expansion.
//!
//! The merged result is the **default** config; requirements layers
//! sit on top via [`crate::validation`].

use std::path::Path;

use crate::paths::{system_config_dir, user_grok_home};
use crate::validation::{load_requirements, load_system_requirements};
use crate::version_overrides::{self, apply_version_overrides};

/// Read and parse a TOML file WITHOUT `$VAR` expansion (empty table if absent).
/// Shared core of [`load_toml_file`] and the hook-layer read.
fn read_toml_file(path: &Path) -> std::io::Result<toml::Value> {
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str::<toml::Value>(&s) {
            Ok(v) => Ok(v),
            Err(e) => {
                // Built from the span, never from Display — Display echoes the
                // offending source line, which may carry a secret. Safe to log and
                // to return to a client.
                let detail = toml_error_detail(&s, &e);
                tracing::error!(file = %path.display(), "config toml has syntax errors: {detail}");
                Err(std::io::Error::other(detail))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(e) => {
            tracing::error!(file = %path.display(), "config file unreadable: {e}");
            Err(e)
        }
    }
}

/// Load and parse a TOML file, expanding `$VAR` references. Empty table if absent.
pub fn load_toml_file(path: &Path) -> std::io::Result<toml::Value> {
    let mut v = read_toml_file(path)?;
    expand_env_vars_in_toml(&mut v);
    Ok(v)
}

/// A snippet-free description of a TOML parse error: `"TOML parse error at line
/// L, column C: <what>"` (or just the message when there's no span). Never
/// includes the offending source line — `Display` echoes it and it may carry a
/// secret — so this is safe to log or surface to a client. Shared with the trace
/// `config_files` artifact so the redaction rule lives in one place.
pub fn toml_error_detail(src: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let (line, col) = line_col(src, span.start);
            format!(
                "TOML parse error at line {line}, column {col}: {}",
                e.message()
            )
        }
        None => e.message().to_owned(),
    }
}

/// 1-based (line, column) of a byte offset within `src`.
fn line_col(src: &str, byte: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// [`load_toml_file`] plus that layer's `[[version_overrides]]`. Use for
/// grok config files; use [`load_toml_file`] directly for unrelated TOML.
pub fn load_config_file(path: &Path) -> std::io::Result<toml::Value> {
    let mut v = load_toml_file(path)?;
    apply_version_overrides_with_registered(&mut v)?;
    Ok(v)
}

pub fn load_from_disk() -> std::io::Result<toml::Value> {
    load_user_config_layer(user_grok_home().as_deref(), USER_CONFIG_FILENAME)
}

/// User config filename (`$GROK_HOME/config.toml`), shared by the loaders here.
pub const USER_CONFIG_FILENAME: &str = "config.toml";

/// Managed config filename, shared by the loaders in this module.
pub const MANAGED_CONFIG_FILENAME: &str = "managed_config.toml";

/// Requirements (cloud-cache) filename — the sibling server-synced artifact.
pub const REQUIREMENTS_FILENAME: &str = "requirements.toml";

pub fn load_managed_config() -> std::io::Result<toml::Value> {
    load_user_config_layer(user_grok_home().as_deref(), MANAGED_CONFIG_FILENAME)
}

/// Load a user-tier config layer from `<home>/<filename>`. With no resolvable
/// user home, returns an empty table rather than reading a cwd-relative
/// `.grok/<filename>` (the cwd-fallback would silently promote an untrusted
/// project `.grok` to the user tier).
fn load_user_config_layer(home: Option<&Path>, filename: &str) -> std::io::Result<toml::Value> {
    match home {
        Some(g) => load_config_file(&g.join(filename)),
        None => Ok(toml::Value::Table(toml::map::Map::new())),
    }
}

pub fn load_system_managed_config() -> std::io::Result<toml::Value> {
    let mut v = match system_config_dir() {
        Some(dir) => load_toml_file(&dir.join(MANAGED_CONFIG_FILENAME))?,
        None => toml::Value::Table(toml::map::Map::new()),
    };
    apply_version_overrides_with_registered(&mut v)?;
    Ok(v)
}

/// One managed-config layer: the parsed TOML and the file it came from.
#[derive(Debug, Clone)]
pub struct ManagedConfigLayer {
    pub value: toml::Value,
    pub path: std::path::PathBuf,
    /// `true` for the root-owned system layer (`/etc/grok`), derived from the
    /// load directory.
    pub is_system: bool,
}

/// All `managed_config.toml` layers in apply order (system first, user last).
/// Absent layers are skipped; unparsable layers are skipped with a warning.
/// One bad layer never drops the others.
pub fn managed_config_layers() -> Vec<ManagedConfigLayer> {
    managed_config_layers_at(system_config_dir().as_deref(), user_grok_home().as_deref())
}

/// [`managed_config_layers`] with explicit directories.
pub fn managed_config_layers_at(
    system_dir: Option<&Path>,
    user_home: Option<&Path>,
) -> Vec<ManagedConfigLayer> {
    let mut layers = Vec::new();
    for (dir, is_system) in [(system_dir, true), (user_home, false)] {
        let Some(path) = dir.map(|d| d.join(MANAGED_CONFIG_FILENAME)) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        match load_config_file(&path) {
            Ok(value) => layers.push(ManagedConfigLayer {
                value,
                path,
                is_system,
            }),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping managed_config.toml layer that failed to load or parse")
            }
        }
    }
    layers
}

/// A hook's origin (held by `xai_grok_hooks::HookSpec::layer`). Defined here, not
/// in `xai-grok-hooks`, since the dep direction is `xai-grok-hooks -> xai-grok-config`;
/// this crate sets the config tiers, `File`/`Plugin` are set downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookProvenance {
    /// `/etc/grok/managed_config.toml`.
    SystemManaged,
    /// `$GROK_HOME/managed_config.toml` (server-synced).
    Managed,
    /// `requirements.toml` (user or system tier).
    Requirements,
    /// `$GROK_HOME/config.toml`.
    User,
    /// A JSON hook file (the hooks directory, a vendor settings file, or a
    /// configured hooks path).
    File,
    /// A plugin-contributed hook.
    Plugin,
    /// A tier this build doesn't recognize (e.g. a newer peer's provenance over
    /// the wire). Forward-tolerant so an unknown value degrades to a
    /// conservative origin instead of failing the whole `HookRegistry` decode.
    #[serde(other)]
    Unknown,
}

/// Defaults to `File` so pre-provenance wire records decode as the most
/// conservative origin.
impl Default for HookProvenance {
    fn default() -> Self {
        Self::File
    }
}

impl HookProvenance {
    /// The snake_case wire string (matches the derived serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemManaged => "system_managed",
            Self::Managed => "managed",
            Self::Requirements => "requirements",
            Self::User => "user",
            Self::File => "file",
            Self::Plugin => "plugin",
            Self::Unknown => "unknown",
        }
    }
}

impl std::str::FromStr for HookProvenance {
    type Err = std::convert::Infallible;

    /// Inverse of [`HookProvenance::as_str`]. Unrecognized strings map to
    /// [`HookProvenance::Unknown`] (forward-tolerant), so this never fails.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "system_managed" => Self::SystemManaged,
            "managed" => Self::Managed,
            "requirements" => Self::Requirements,
            "user" => Self::User,
            "file" => Self::File,
            "plugin" => Self::Plugin,
            _ => Self::Unknown,
        })
    }
}

/// One config layer's `hooks` subtree (read without `$VAR` expansion) plus its
/// provenance.
#[derive(Debug, Clone)]
pub struct HookConfigLayer {
    provenance: HookProvenance,
    source_name: String,
    path: std::path::PathBuf,
    hooks: toml::Value,
}

impl HookConfigLayer {
    /// Construct a layer directly (in-memory config and tests); the synthesized
    /// `path` mirrors `source_name`. The normal path is [`hook_config_layers`].
    pub fn new(
        provenance: HookProvenance,
        source_name: impl Into<String>,
        hooks: toml::Value,
    ) -> Self {
        let source_name = source_name.into();
        let path = std::path::PathBuf::from(&source_name);
        Self {
            provenance,
            source_name,
            path,
            hooks,
        }
    }

    pub fn provenance(&self) -> HookProvenance {
        self.provenance
    }

    /// A stable label for this layer (e.g. `"managed"`, `"requirements/user"`),
    /// used to prefix hook names for display and dedup.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// The layer's backing file, so parse errors can cite a real path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The raw `hooks` table, unexpanded so a literal `${VAR}` reaches the runner.
    pub fn hooks(&self) -> &toml::Value {
        &self.hooks
    }
}

/// All config-layer `hooks` blocks, highest authority first (matching
/// [`effective_config_base`]). Read WITHOUT env-expansion and never merged (hooks
/// combine additively downstream); absent/unparsable layers are skipped with a
/// warning so one bad layer can't drop the others. macOS MDM is excluded (not a
/// TOML file; MDM hooks belong to the enforcement work).
pub fn hook_config_layers() -> Vec<HookConfigLayer> {
    hook_config_layers_at(system_config_dir().as_deref(), user_grok_home().as_deref())
}

/// [`hook_config_layers`] with explicit directories, for tests.
pub fn hook_config_layers_at(
    system_dir: Option<&Path>,
    user_home: Option<&Path>,
) -> Vec<HookConfigLayer> {
    /// One candidate config-hook layer: which directory + filename to read, and
    /// the provenance/label to stamp on hooks found there.
    struct LayerSpec<'a> {
        dir: Option<&'a Path>,
        filename: &'a str,
        provenance: HookProvenance,
        source_name: &'a str,
    }

    // Highest config authority first, matching `effective_config_base` precedence
    // (requirements > user > managed > system_managed; user overrides managed in
    // this model). Order only affects which label a byte-identical duplicate keeps
    // under first-wins dedup; every distinct hook runs regardless.
    let specs = [
        LayerSpec {
            dir: system_dir,
            filename: REQUIREMENTS_FILENAME,
            provenance: HookProvenance::Requirements,
            source_name: "requirements/system",
        },
        LayerSpec {
            dir: user_home,
            filename: REQUIREMENTS_FILENAME,
            provenance: HookProvenance::Requirements,
            source_name: "requirements/user",
        },
        LayerSpec {
            dir: user_home,
            filename: USER_CONFIG_FILENAME,
            provenance: HookProvenance::User,
            source_name: "user",
        },
        LayerSpec {
            dir: user_home,
            filename: MANAGED_CONFIG_FILENAME,
            provenance: HookProvenance::Managed,
            source_name: "managed",
        },
        LayerSpec {
            dir: system_dir,
            filename: MANAGED_CONFIG_FILENAME,
            provenance: HookProvenance::SystemManaged,
            source_name: "system_managed",
        },
    ];

    let mut layers = Vec::new();
    for LayerSpec {
        dir,
        filename,
        provenance,
        source_name,
    } in specs
    {
        let Some(path) = dir.map(|d| d.join(filename)) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        // No `$VAR` expansion: a literal `${VAR}` must reach the hook runner, which
        // does the single expansion (expanding here would double-expand).
        let mut value = match read_toml_file(&path) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping config layer whose hooks could not be read");
                continue;
            }
        };
        // Apply `[[version_overrides]]` (parity with `load_config_file`); deep-merge
        // only, no `$VAR` expansion, so the raw-read invariant holds.
        if let Err(e) = apply_version_overrides_with_registered(&mut value) {
            tracing::warn!(path = %path.display(), error = %e, "skipping config layer whose version_overrides failed to apply");
            continue;
        }
        let Some(hooks) = value.get("hooks") else {
            continue;
        };
        if !hooks.is_table() {
            tracing::warn!(path = %path.display(), "ignoring non-table `hooks` value in config layer");
            continue;
        }
        layers.push(HookConfigLayer {
            provenance,
            source_name: source_name.to_string(),
            path: path.clone(),
            hooks: hooks.clone(),
        });
    }
    layers
}

/// Layers lowest→highest priority. `[[campaigns]]` taken off each layer at load.
#[derive(Clone)]
pub struct ConfigLayers {
    pub system_managed: toml::Value,
    pub managed: toml::Value,
    pub user: toml::Value,
    pub user_requirements: Option<toml::Value>,
    pub system_requirements: Option<toml::Value>,
    /// macOS MDM requirements; highest requirements tier when present.
    pub mdm_requirements: Option<toml::Value>,
    pub campaigns: crate::campaigns::CampaignOverrides,
}

impl Default for ConfigLayers {
    fn default() -> Self {
        Self {
            system_managed: toml::Value::Table(Default::default()),
            managed: toml::Value::Table(Default::default()),
            user: toml::Value::Table(Default::default()),
            user_requirements: None,
            system_requirements: None,
            mdm_requirements: None,
            campaigns: crate::campaigns::CampaignOverrides::default(),
        }
    }
}

impl ConfigLayers {
    pub fn load() -> std::io::Result<Self> {
        use crate::campaigns::{CampaignOverrides, take_campaign_entries};

        let mut system_managed = load_system_managed_config()?;
        let system_managed_campaigns = take_campaign_entries(&mut system_managed, "system_managed");

        let mut managed = load_managed_config()?;
        let managed_campaigns = take_campaign_entries(&mut managed, "managed");

        let mut user = load_from_disk()?;
        let user_campaigns = take_campaign_entries(&mut user, "user");

        let mut user_requirements = load_requirements();
        let mut system_requirements = load_system_requirements();
        let mut mdm_requirements = crate::validation::mdm_requirements_value();

        // Highest-authority requirements tier first: `merge_campaign_entries` is
        // first-id-wins, so a duplicate campaign id must resolve mdm > system >
        // user — matching the layer precedence in `effective_config_base` (where
        // mdm is merged last/highest).
        let mut requirements_campaigns = Vec::new();
        if let Some(ref mut req) = mdm_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }
        if let Some(ref mut req) = system_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }
        if let Some(ref mut req) = user_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }

        Ok(Self {
            system_managed,
            managed,
            user,
            user_requirements,
            system_requirements,
            mdm_requirements,
            campaigns: CampaignOverrides {
                requirements: requirements_campaigns,
                user: user_campaigns,
                managed: managed_campaigns,
                system_managed: system_managed_campaigns,
            },
        })
    }

    /// Layer merge only (no campaign overlay).
    pub fn effective_config_base(&self) -> toml::Value {
        let mut merged = self.system_managed.clone();
        deep_merge_toml(&mut merged, &self.managed);
        deep_merge_toml(&mut merged, &self.user);
        if let Some(req) = &self.user_requirements {
            deep_merge_toml(&mut merged, req);
        }
        if let Some(sys_req) = &self.system_requirements {
            deep_merge_toml(&mut merged, sys_req);
        }
        if let Some(mdm_req) = &self.mdm_requirements {
            deep_merge_toml(&mut merged, mdm_req);
        }
        merged
    }

    /// Campaign source slices in priority order (first id wins):
    /// requirements > remote > user > managed > system_managed. Single source of
    /// truth for the precedence; both this crate and the shell resolver consume it.
    pub fn campaign_source_slices<'a>(
        &'a self,
        remote_campaigns: &'a [crate::campaigns::CampaignEntry],
    ) -> [&'a [crate::campaigns::CampaignEntry]; 5] {
        [
            &self.campaigns.requirements,
            remote_campaigns,
            &self.campaigns.user,
            &self.campaigns.managed,
            &self.campaigns.system_managed,
        ]
    }

    /// Active campaigns against `base`: kill switch → priority merge (first-id-wins)
    /// → drop dismissed. The single place disk campaign resolution lives; the shell
    /// wraps this with the `GROK_CAMPAIGNS_OVERRIDE` env layer.
    pub fn resolve_campaigns(
        &self,
        base: &toml::Value,
        remote_campaigns: &[crate::campaigns::CampaignEntry],
        dismissed_ids: &std::collections::HashSet<String>,
    ) -> Vec<crate::campaigns::CampaignEntry> {
        if campaigns_application_disabled(base) {
            return Vec::new();
        }
        let merged = crate::campaigns::merge_campaign_entries(
            &self.campaign_source_slices(remote_campaigns),
        );
        crate::campaigns::filter_active_campaigns(merged, dismissed_ids)
    }

    /// Re-merge the requirements layers so an admin's `requirements.toml` always
    /// wins over a campaign overlay, regardless of the campaign's source layer.
    /// Campaigns are full-power (any field), so this is the structural guarantee
    /// that a lower-trust layer's campaign can't override an admin-set field.
    fn reapply_requirements(&self, merged: &mut toml::Value) {
        for req in [
            &self.user_requirements,
            &self.system_requirements,
            &self.mdm_requirements,
        ]
        .into_iter()
        .flatten()
        {
            deep_merge_toml(merged, req);
        }
    }

    /// Apply active campaign patches onto `merged`, then restore requirements
    /// precedence. The single overlay step shared by this crate and the shell.
    pub fn apply_campaign_overrides(
        &self,
        merged: &mut toml::Value,
        active: &[crate::campaigns::CampaignEntry],
    ) {
        crate::campaigns::apply_active_campaign_patches(merged, active);
        self.reapply_requirements(merged);
    }

    /// Layer merge + disk/remote campaign overlay, honoring the kill switch. The
    /// shell's `load_effective_config` is the remote/override-aware path; this is
    /// used by `effective_config_disk_only` and tests.
    pub fn effective_config_with_campaigns(
        &self,
        remote_campaigns: &[crate::campaigns::CampaignEntry],
        dismissed_ids: &std::collections::HashSet<String>,
    ) -> toml::Value {
        let mut merged = self.effective_config_base();
        let active = self.resolve_campaigns(&merged, remote_campaigns, dismissed_ids);
        self.apply_campaign_overrides(&mut merged, &active);
        merged
    }

    /// Disk campaigns + on-disk dismiss (`campaigns_state.json`); **no remote, no
    /// env override**. Named to make the divergence from the shell's remote-aware
    /// `load_effective_config` explicit at every call site.
    pub fn effective_config_disk_only(&self) -> toml::Value {
        self.effective_config_with_campaigns(&[], &load_dismissed_ids_from_home())
    }

    pub fn has_managed(&self) -> bool {
        self.managed.as_table().is_some_and(|t| !t.is_empty())
            || self
                .system_managed
                .as_table()
                .is_some_and(|t| !t.is_empty())
    }

    pub fn has_system_managed(&self) -> bool {
        self.system_managed
            .as_table()
            .is_some_and(|t| !t.is_empty())
    }
}

/// `GROK_CAMPAIGNS=0` or `[features] campaigns = false` on pre-campaign base.
pub fn campaigns_application_disabled(base_effective: &toml::Value) -> bool {
    if crate::env_bool("GROK_CAMPAIGNS") == Some(false) {
        return true;
    }
    base_effective
        .get("features")
        .and_then(|f| f.get("campaigns"))
        .and_then(|c| c.as_bool())
        == Some(false)
}

/// Disk layers only (no remote, no env override). Prefer the shell loader
/// (`xai_grok_shell::util::config::load_effective_config`) when remote campaigns
/// or `GROK_CAMPAIGNS_OVERRIDE` must be honored. The name mirrors the
/// [`ConfigLayers::effective_config_disk_only`] method so the divergence from the
/// remote-aware loader is un-ignorable at every call site.
pub fn load_effective_config_disk_only() -> std::io::Result<toml::Value> {
    Ok(ConfigLayers::load()?.effective_config_disk_only())
}

/// On-disk campaign dismiss state. Single source of truth for the file's name,
/// location, and JSON shape — the shell's writer reuses these so the read and
/// write sides can't drift.
pub const CAMPAIGNS_STATE_FILE: &str = "campaigns_state.json";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CampaignsState {
    #[serde(default)]
    pub dismissed_ids: Vec<String>,
}

/// Path to `$GROK_HOME/campaigns_state.json` under `home`.
pub fn campaigns_state_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(CAMPAIGNS_STATE_FILE)
}

/// Fail-open dismissed ids from `$GROK_HOME/campaigns_state.json`.
pub fn load_dismissed_ids_from_home() -> std::collections::HashSet<String> {
    let Some(home) = crate::user_grok_home() else {
        return std::collections::HashSet::new();
    };
    let Ok(contents) = std::fs::read_to_string(campaigns_state_path(&home)) else {
        return std::collections::HashSet::new();
    };
    serde_json::from_str::<CampaignsState>(&contents)
        .map(|s| s.dismissed_ids.into_iter().collect())
        .unwrap_or_default()
}

/// Applies matching `[[version_overrides]]` patches against the running
/// CLI version; strips the section either way. If the installed version
/// can't be parsed (broken `GROK_TEST_VERSION` in dev), silently strips
/// without applying — keeps the CLI usable on a bad dev override.
pub fn apply_version_overrides_with_registered(value: &mut toml::Value) -> std::io::Result<()> {
    match xai_grok_version::installed_semver() {
        Ok(version) => apply_version_overrides(value, &version)
            .map_err(|e| std::io::Error::other(e.to_string())),
        Err(_) => {
            if let Some(table) = value.as_table_mut() {
                table.remove(version_overrides::VERSION_OVERRIDES_KEY);
            }
            Ok(())
        }
    }
}

/// Recursively merge `overrides` into `base`. Values in `overrides` win.
pub fn deep_merge_toml(base: &mut toml::Value, overrides: &toml::Value) {
    if let toml::Value::Table(overrides_table) = overrides
        && let toml::Value::Table(base_table) = base
    {
        for (key, value) in overrides_table {
            if let Some(existing) = base_table.get_mut(key) {
                deep_merge_toml(existing, value);
            } else {
                base_table.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overrides.clone();
    }
}

/// Expand `$VAR` / `${VAR}` in all string values.
pub fn expand_env_vars_in_toml(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => {
            let expanded = expand_env_vars_in_string(s);
            if expanded != *s {
                *s = expanded;
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                expand_env_vars_in_toml(item);
            }
        }
        toml::Value::Table(table) => {
            for (_, item) in table.iter_mut() {
                expand_env_vars_in_toml(item);
            }
        }
        _ => {}
    }
}

/// Expand `$VAR` / `${VAR}` in a single string.
pub fn expand_env_vars_in_string(input: &str) -> String {
    let context = |name: &str| std::env::var(name).ok();
    shellexpand::env_with_context_no_errors(input, context).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn hook_config_layers_reads_each_layer_unmerged_with_provenance() {
        let sys = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            "config.toml",
            "[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"${HOME}/u.sh\"\n",
        );
        write(
            home.path(),
            MANAGED_CONFIG_FILENAME,
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"/m.sh\"\n",
        );
        write(
            sys.path(),
            REQUIREMENTS_FILENAME,
            "[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype = \"command\"\ncommand = \"/r.sh\"\n",
        );

        let layers = hook_config_layers_at(Some(sys.path()), Some(home.path()));

        // Highest authority first, each layer keeping its own provenance.
        let names: Vec<_> = layers.iter().map(|l| l.source_name().to_string()).collect();
        assert_eq!(names, vec!["requirements/system", "user", "managed"]);
        assert_eq!(layers[1].provenance(), HookProvenance::User);
        // Unmerged, and `${HOME}` stays literal (the runner expands, not the loader).
        let cmd = layers[1].hooks()["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "${HOME}/u.sh");
    }

    #[test]
    fn hook_config_layers_bad_user_layer_does_not_drop_managed() {
        // A broken user config.toml must not drop the admin managed layer.
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "config.toml", "this is = = not valid toml");
        write(
            home.path(),
            MANAGED_CONFIG_FILENAME,
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"/m.sh\"\n",
        );
        let layers = hook_config_layers_at(None, Some(home.path()));
        let names: Vec<_> = layers.iter().map(|l| l.source_name().to_string()).collect();
        assert_eq!(names, vec!["managed"]);
    }

    #[test]
    fn full_layer_precedence_requirements_over_config_over_managed() {
        let system_managed: toml::Value =
            toml::from_str("[telemetry]\nmode = \"system_managed_value\"\n").unwrap();
        let managed: toml::Value =
            toml::from_str("[telemetry]\nmode = \"managed_value\"\n").unwrap();
        let user: toml::Value = toml::from_str("[telemetry]\nmode = \"user_value\"\n").unwrap();
        let user_requirements: toml::Value =
            toml::from_str("[telemetry]\nmode = \"user_requirements_value\"\n").unwrap();
        let system_requirements: toml::Value =
            toml::from_str("[telemetry]\nmode = \"system_requirements_value\"\n").unwrap();

        let mut merged = system_managed;
        deep_merge_toml(&mut merged, &managed);
        deep_merge_toml(&mut merged, &user);
        deep_merge_toml(&mut merged, &user_requirements);
        deep_merge_toml(&mut merged, &system_requirements);

        assert_eq!(
            merged["telemetry"]["mode"].as_str(),
            Some("system_requirements_value")
        );
    }

    #[test]
    fn effective_config_mdm_requirements_win_over_system_and_user() {
        // MDM is merged last, so an admin-forced value clamps the effective
        // config over both the user config and the system requirements layer.
        let layers = ConfigLayers {
            user: toml::from_str("[features]\nweb_fetch = true\n").unwrap(),
            system_requirements: Some(toml::from_str("[features]\nweb_fetch = true\n").unwrap()),
            mdm_requirements: Some(toml::from_str("[features]\nweb_fetch = false\n").unwrap()),
            ..Default::default()
        };
        assert_eq!(
            layers.effective_config_disk_only()["features"]["web_fetch"].as_bool(),
            Some(false),
        );
    }

    #[test]
    fn full_precedence_holds_when_values_come_from_version_overrides() {
        let cli_version = semver::Version::parse("1.8.0").unwrap();

        let mut managed: toml::Value = toml::from_str(
            r#"
            [[version_overrides]]
            minimum_version = "1.0.0"
            [version_overrides.telemetry]
            mode = "managed_versioned"
            "#,
        )
        .unwrap();
        let mut user: toml::Value = toml::from_str(
            r#"
            [[version_overrides]]
            minimum_version = "1.0.0"
            [version_overrides.telemetry]
            mode = "user_versioned"
            "#,
        )
        .unwrap();
        let mut requirements: toml::Value = toml::from_str(
            r#"
            [[version_overrides]]
            minimum_version = "1.0.0"
            [version_overrides.telemetry]
            mode = "requirements_versioned"
            "#,
        )
        .unwrap();

        apply_version_overrides(&mut managed, &cli_version).unwrap();
        apply_version_overrides(&mut user, &cli_version).unwrap();
        apply_version_overrides(&mut requirements, &cli_version).unwrap();

        let mut merged = managed;
        deep_merge_toml(&mut merged, &user);
        deep_merge_toml(&mut merged, &requirements);

        assert_eq!(
            merged["telemetry"]["mode"].as_str(),
            Some("requirements_versioned")
        );
    }

    /// Direct contract for `deep_merge_toml`: nested tables merge (siblings
    /// preserved), arrays replace (not concatenate), missing keys insert.
    #[test]
    fn deep_merge_toml_table_merge_array_replace_and_insert() {
        let mut base: toml::Value = toml::from_str(
            r#"
            [features.telemetry]
            enabled = false
            sample_rate = 0.0

            [server]
            allowed = ["a", "b"]
            "#,
        )
        .unwrap();
        let overrides: toml::Value = toml::from_str(
            r#"
            [features.telemetry]
            enabled = true

            [server]
            allowed = ["c"]

            [brand_new]
            x = 1
            "#,
        )
        .unwrap();

        deep_merge_toml(&mut base, &overrides);

        assert_eq!(
            base["features"]["telemetry"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            base["features"]["telemetry"]["sample_rate"].as_float(),
            Some(0.0)
        );
        let arr: Vec<_> = base["server"]["allowed"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(arr, vec!["c"]);
        assert_eq!(base["brand_new"]["x"].as_integer(), Some(1));
    }

    #[test]
    fn user_version_overrides_dont_escape_their_layer() {
        let cli_version = semver::Version::parse("1.8.0").unwrap();
        let mut user: toml::Value = toml::from_str(
            r#"
            [[version_overrides]]
            minimum_version = "1.0.0"
            [version_overrides.telemetry]
            mode = "enabled"
            "#,
        )
        .unwrap();
        apply_version_overrides(&mut user, &cli_version).unwrap();
        assert_eq!(user["telemetry"]["mode"].as_str(), Some("enabled"));

        let requirements: toml::Value = toml::from_str(
            r#"
            [telemetry]
            mode = "disabled"
            "#,
        )
        .unwrap();

        let mut merged = user;
        deep_merge_toml(&mut merged, &requirements);
        assert_eq!(merged["telemetry"]["mode"].as_str(), Some("disabled"));
    }

    #[test]
    fn load_user_config_layer_is_empty_without_user_home() {
        // No resolvable user home: no user layer, and crucially no
        // cwd-relative .grok read.
        let v = load_user_config_layer(None, "config.toml").unwrap();
        assert_eq!(v.as_table().map(|t| t.is_empty()), Some(true));
    }

    #[test]
    fn load_user_config_layer_reads_file_when_home_present() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("grok-load-layer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("config.toml")).unwrap();
        writeln!(f, "[telemetry]\nmode = \"from_file\"\n").unwrap();

        let v = load_user_config_layer(Some(&dir), "config.toml").unwrap();
        assert_eq!(v["telemetry"]["mode"].as_str(), Some("from_file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The returned error keeps the parser's kind + location but never the source
    /// snippet, which can carry a secret and would reach a client caller.
    #[test]
    fn parse_error_keeps_kind_but_not_snippet() {
        let dir = std::env::temp_dir().join(format!("grok-toml-leak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        // Duplicate key: the message names the key; the secret-bearing source line is only in Display.
        std::fs::write(
            &path,
            "api_key = \"xai-secretmustnotleak\"\napi_key = \"xai-secretmustnotleak2\"\n",
        )
        .unwrap();

        let msg = load_toml_file(&path).unwrap_err().to_string();
        assert!(
            msg.contains("TOML parse error at line 2"),
            "want location: {msg}"
        );
        assert!(msg.contains("duplicate key"), "want parser kind: {msg}");
        assert!(
            !msg.contains("xai-secretmustnotleak"),
            "leaked the secret value: {msg}"
        );
        assert!(
            !msg.contains('|') && !msg.contains('^'),
            "leaked the source snippet/caret: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `GROK_CAMPAIGNS=0` disables campaign application regardless of config.
    /// `GROK_CAMPAIGNS` is process-global, so this test serializes itself with a
    /// module-local mutex and save/restores the prior value. (This crate has no
    /// `serial_test` dev-dep and no other test reads this var, so a local guard
    /// is sufficient.)
    #[test]
    fn kill_switch_env_var_disables() {
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("GROK_CAMPAIGNS");
        let empty = toml::Value::Table(Default::default());

        // SAFETY: ENV_GUARD serializes this against itself; no other test in the
        // crate mutates or reads GROK_CAMPAIGNS concurrently.
        unsafe { std::env::set_var("GROK_CAMPAIGNS", "0") };
        assert!(campaigns_application_disabled(&empty));

        unsafe { std::env::remove_var("GROK_CAMPAIGNS") };
        assert!(!campaigns_application_disabled(&empty));

        match prior {
            Some(v) => unsafe { std::env::set_var("GROK_CAMPAIGNS", v) },
            None => unsafe { std::env::remove_var("GROK_CAMPAIGNS") },
        }
    }
}
