//! Whether this process may keep a session-search index. The index crate reads it as
//! [`is_index_enabled`] and does the enforcing there.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use xai_grok_config_types::{ConfigSource, Resolved};

const UNAPPLIED: u8 = 0;
const OPEN: u8 = 1;
const CLOSED: u8 = 2;

/// One latch for the process, so in leader mode the first workspace to turn search off turns it
/// off for every workspace hosted beside it. The agent resolves the setting in `initialize` and
/// `grok sessions` before it searches; a reader that arrives before either falls back to disk.
static GATE: AtomicU8 = AtomicU8::new(UNAPPLIED);

static CLOSED_BY: OnceLock<ConfigSource> = OnceLock::new();

/// Off only: the completed-bootstrap marker outlives the time spent off, so turning it back on
/// here would serve an index missing everything written meanwhile.
pub(crate) fn apply_gate(setting: &Resolved<bool>) {
    if !setting.value {
        let _ = CLOSED_BY.set(setting.source);
        if GATE.swap(CLOSED, Ordering::AcqRel) != CLOSED {
            tracing::info!(
                source = %setting.source,
                "session search index turned off for this process"
            );
        }
        return;
    }
    let opened = GATE.compare_exchange(UNAPPLIED, OPEN, Ordering::AcqRel, Ordering::Acquire);
    if opened == Err(CLOSED) {
        tracing::info!(source = %setting.source, "session search stays off until the next launch");
    }
}

/// Names the setting that turned search off, for a message like `off (a requirements.toml pin)`.
pub(crate) fn session_search_off_reason(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Requirement => "a requirements.toml pin or an MDM policy",
        ConfigSource::Env => "the GROK_SESSION_SEARCH environment variable",
        ConfigSource::Remote => "a remote setting",
        ConfigSource::Config
        | ConfigSource::UserConfig
        | ConfigSource::ManagedConfig
        | ConfigSource::SystemManagedConfig => "the session_search key in a Grok config file",
        // Neither can resolve to off: the default is on and no flag sets this key.
        ConfigSource::Cli | ConfigSource::Default => "a local setting",
    }
}

pub(crate) fn closed_by() -> Option<ConfigSource> {
    CLOSED_BY.get().copied()
}

/// Cheap after the setting is resolved. The first call in a process that never applied it reads
/// the config files from disk.
pub(crate) fn is_index_enabled() -> bool {
    match GATE.load(Ordering::Acquire) {
        CLOSED => false,
        OPEN => true,
        // Nothing has resolved the setting yet, so resolve the disk tiers here rather than assume
        // on: a pin still outranks the environment. Remote settings are the tier this misses.
        _ => {
            let setting = match crate::config::load_agent_config_disk_only() {
                Ok(config) => config.resolve_session_search(),
                // The whole-config load is all or nothing, but a corrupt user-writable
                // config.toml must not disarm a pin, so read the tiers that stand on their own.
                Err(e) => {
                    tracing::warn!(error = %e, "could not read the config for session search");
                    let env = xai_grok_config::env_bool("GROK_SESSION_SEARCH");
                    match (requirements_pin(), env) {
                        (Some(pinned), _) => Resolved::new(pinned, ConfigSource::Requirement),
                        (None, Some(set)) => Resolved::new(set, ConfigSource::Env),
                        (None, None) => Resolved::new(true, ConfigSource::Default),
                    }
                }
            };
            tracing::debug!(
                enabled = setting.value,
                "session search resolved from disk before anything applied the setting"
            );
            // Latch it, so this work happens once, and report the latch rather than the value:
            // another thread may have closed the gate while the config was loading.
            apply_gate(&setting);
            GATE.load(Ordering::Acquire) != CLOSED
        }
    }
}

/// The requirements layers, read alone when the merged config will not load. A user has one of
/// these too; the system and MDM layers win over it.
fn requirements_pin() -> Option<bool> {
    crate::config::load_merged_requirements()?
        .get("features")?
        .get("session_search")?
        .as_bool()
}

/// Holders must be `#[serial]`. Restores `GATE` but not `CLOSED_BY`, which is set once per process.
#[cfg(test)]
#[must_use]
pub(crate) struct IndexGateGuard {
    prior: u8,
}

#[cfg(test)]
impl IndexGateGuard {
    pub(crate) fn snapshot() -> Self {
        Self {
            prior: GATE.load(Ordering::Acquire),
        }
    }

    pub(crate) fn open() -> Self {
        let guard = Self::snapshot();
        GATE.store(OPEN, Ordering::Release);
        guard
    }
}

#[cfg(test)]
impl Drop for IndexGateGuard {
    fn drop(&mut self) {
        GATE.store(self.prior, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "search_gate_tests.rs"]
mod tests;
