//! Hyper harness model catalog + reasoning ladder.
//!
//! `models()` is LIVE: spawn `hyper agent stdio`, `initialize`, read
//! `modelState` from the init `_meta`, build `Vec<Model>`. Only **usable**
//! models are returned — locked BYOK entries stamped with `requiresApiKey` /
//! `requiresOAuth` (no credentials) are omitted so the desktop picker never
//! offers models the user cannot run.
//!
//! Results are TTL-cached briefly; call [`invalidate_models_cache`] after
//! login / API-key changes so the next ListModels re-probes.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use comet_proto::agent::{Model, ReasoningLevel};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::HarnessError;

/// Reasoning levels hyper exposes (mirrors the thought-level config options
/// hyper's ACP surface reports — `session/set_config_option` thought-level).
pub const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

/// How long a successful live catalog is reused without re-spawning hyper.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(30);

struct ModelsCache {
    models: Vec<Model>,
    fetched_at: Instant,
}

static MODELS_CACHE: Mutex<Option<ModelsCache>> = Mutex::new(None);
/// Bumped on every invalidate so the UI can drop its Ready list and re-fetch.
static MODELS_CACHE_GEN: AtomicU64 = AtomicU64::new(0);

/// Drop the cached Hyper model list (e.g. after login or API-key save).
pub fn invalidate_models_cache() {
    if let Ok(mut guard) = MODELS_CACHE.lock() {
        *guard = None;
    }
    MODELS_CACHE_GEN.fetch_add(1, Ordering::SeqCst);
}

/// Monotonic generation for UI invalidation after credentials change.
pub fn models_cache_generation() -> u64 {
    MODELS_CACHE_GEN.load(Ordering::SeqCst)
}

/// Parse the live model catalog from the ACP `initialize` `_meta.modelState`.
///
/// Skips locked platform rows (requiresApiKey / requiresOAuth) so the desktop
/// only lists models that are currently runnable with the user's credentials.
pub(super) fn models_from_meta(meta: &Option<acp::Meta>) -> Vec<Model> {
    let Some(meta) = meta.as_ref() else {
        return Vec::new();
    };
    let Some(ms) = meta
        .get("modelState")
        .or_else(|| meta.get("model_state"))
    else {
        return Vec::new();
    };
    let Some(available) = ms
        .get("availableModels")
        .or_else(|| ms.get("available_models"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for m in available {
        if model_entry_locked(m) {
            continue;
        }
        let id = m
            .get("modelId")
            .or_else(|| m.get("model_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let label = m
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let description = m
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        models.push(Model {
            id,
            label,
            description,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: Vec::new(),
        });
    }
    models
}

/// True when ACP stamped this row as needing setup (not currently usable).
fn model_entry_locked(m: &serde_json::Value) -> bool {
    // Meta can live on the model object itself or under `.meta`.
    let meta = m.get("meta").or(Some(m));
    let Some(meta) = meta else {
        return false;
    };
    let requires_api_key = meta
        .get("requiresApiKey")
        .or_else(|| meta.get("requires_api_key"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let requires_oauth = meta
        .get("requiresOAuth")
        .or_else(|| meta.get("requires_oauth"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    requires_api_key || requires_oauth
}

/// Providers the user has actually configured (OAuth and/or API keys).
#[derive(Debug, Default, Clone)]
struct ConfiguredProviders {
    /// First-party xAI / Grok session or `XAI_API_KEY`.
    xai: bool,
    /// Managed catalog prefixes: `openrouter`, `openai-codex`, `kimi-code`, …
    platforms: HashSet<String>,
}

impl ConfiguredProviders {
    fn is_empty(&self) -> bool {
        !self.xai && self.platforms.is_empty()
    }

    fn allows_model(&self, model_id: &str) -> bool {
        if let Some((provider, rest)) = model_id.split_once('/') {
            if rest.is_empty() {
                return false;
            }
            // Managed multi-provider id: `openrouter/…`, `openai-codex/…`.
            return self.platforms.contains(provider);
        }
        // Bare id (`grok-4`, …) → first-party xAI only.
        self.xai
    }
}

fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".grok")
        })
}

/// Read `~/.grok/auth.json` (+ `XAI_API_KEY`) to learn which providers are live.
fn configured_providers() -> ConfiguredProviders {
    let mut cfg = ConfiguredProviders::default();
    if std::env::var_os("XAI_API_KEY")
        .filter(|s| !s.is_empty())
        .is_some()
    {
        cfg.xai = true;
    }
    let path = grok_home().join("auth.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return cfg;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return cfg;
    };
    let Some(obj) = value.as_object() else {
        return cfg;
    };

    // Scoped store (normal): keys are auth scopes.
    let looks_scoped = obj.values().any(|v| {
        v.as_object().is_some_and(|o| {
            o.contains_key("auth_mode") || o.contains_key("key") || o.contains_key("refresh_token")
        })
    });
    if looks_scoped {
        for (scope, entry) in obj {
            // Skip empty placeholders.
            let has_secret = entry
                .get("key")
                .and_then(|v| v.as_str())
                .is_some_and(|k| !k.is_empty())
                || entry.get("refresh_token").and_then(|v| v.as_str()).is_some()
                || entry
                    .get("aws_credential_chain")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                || entry
                    .get("aws_profile")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| !p.is_empty());
            if !has_secret {
                continue;
            }
            apply_auth_scope(&mut cfg, scope);
        }
        return cfg;
    }

    // Flat legacy auth.json → treat as xAI if it looks like a login.
    if obj.contains_key("refresh_token")
        || obj.contains_key("user_id")
        || obj.contains_key("account_id")
        || obj.get("key").and_then(|v| v.as_str()).is_some_and(|k| !k.is_empty())
    {
        cfg.xai = true;
    }
    cfg
}

fn apply_auth_scope(cfg: &mut ConfiguredProviders, scope: &str) {
    if scope.starts_with("https://auth.x.ai")
        || scope.starts_with("https://accounts.x.ai")
        || scope == "https://accounts.x.ai/sign-in"
        || scope == "xai::api_key"
    {
        cfg.xai = true;
        return;
    }
    if let Some(rest) = scope.strip_prefix("oauth/") {
        // oauth/openai-codex → openai-codex catalog prefix
        cfg.platforms.insert(rest.to_string());
        return;
    }
    if let Some(platform) = scope.strip_prefix("platform/") {
        cfg.platforms.insert(platform.to_string());
        // xai-direct is first-party API flavor.
        if platform == "xai-direct" || platform == "xai" {
            cfg.xai = true;
        }
    }
}

fn filter_models_for_configured_providers(models: Vec<Model>) -> Vec<Model> {
    let providers = configured_providers();
    if providers.is_empty() {
        tracing::info!(
            "no Hyper providers configured in auth.json; model list empty until Accounts login/API key"
        );
        return Vec::new();
    }
    let before = models.len();
    let filtered: Vec<Model> = models
        .into_iter()
        .filter(|m| providers.allows_model(&m.id))
        .collect();
    if filtered.len() != before {
        tracing::debug!(
            before,
            after = filtered.len(),
            xai = providers.xai,
            platforms = ?providers.platforms,
            "filtered Hyper models to configured providers only"
        );
    }
    filtered
}

/// Live `models()` — TTL-cached. Spawns `hyper agent stdio`, runs `initialize`,
/// and parses `modelState`. Returns only models for **configured** providers
/// (Accounts OAuth / API keys), never locked stubs or invent-ed defaults.
pub async fn live_models() -> Result<Vec<Model>, HarnessError> {
    if let Ok(guard) = MODELS_CACHE.lock() {
        if let Some(cache) = guard.as_ref() {
            if cache.fetched_at.elapsed() < MODELS_CACHE_TTL {
                // Re-apply auth filter in case credentials changed within TTL
                // without invalidate (cheap; auth.json is tiny).
                return Ok(filter_models_for_configured_providers(cache.models.clone()));
            }
        }
    }

    let models = fetch_live_models().await?;
    // Always re-filter against current auth.json (Accounts providers).
    let models = filter_models_for_configured_providers(models);

    if let Ok(mut guard) = MODELS_CACHE.lock() {
        *guard = Some(ModelsCache {
            models: models.clone(),
            fetched_at: Instant::now(),
        });
    }
    Ok(models)
}

async fn fetch_live_models() -> Result<Vec<Model>, HarnessError> {
    let exe = match super::ensure::ensure_hyper_bin().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "hyper ensure failed; model picker will be empty until CLI is available"
            );
            return Ok(Vec::new());
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("hyper-models".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(HarnessError::Io(e)));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let result = local.block_on(&rt, async move {
                let mut child = match tokio::process::Command::new(&exe)
                    .args(["agent", "stdio"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::inherit())
                    .kill_on_drop(true)
                    .env("HYPER_AGENT_BIN", &exe)
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return match e.kind() {
                            std::io::ErrorKind::NotFound => {
                                Err(HarnessError::NotInstalled(exe.display().to_string()))
                            }
                            _ => Err(HarnessError::Io(e)),
                        };
                    }
                };
                let outgoing = match child.stdin.take() {
                    Some(s) => s.compat_write(),
                    None => return Ok(Vec::new()),
                };
                let incoming = match child.stdout.take() {
                    Some(s) => s.compat(),
                    None => return Ok(Vec::new()),
                };
                let result = super::init_meta_over(incoming, outgoing).await;
                let _ = child.start_kill();
                result
            });
            let _ = tx.send(result);
        })
        .map_err(|e| HarnessError::Io(std::io::Error::other(e)))?;
    rx.await
        .map_err(|_| HarnessError::Protocol("models thread dropped".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_from_meta_skips_locked_requires_api_key() {
        let meta: acp::Meta = serde_json::json!({
            "modelState": {
                "currentModelId": "grok-4",
                "availableModels": [
                    { "modelId": "grok-4", "name": "Grok 4" },
                    {
                        "modelId": "openrouter/qwen",
                        "name": "Qwen",
                        "meta": { "requiresApiKey": true }
                    },
                    {
                        "modelId": "openai-codex/gpt",
                        "name": "Codex",
                        "requiresOAuth": true
                    }
                ]
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let models = models_from_meta(&Some(meta));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-4");
    }

    #[test]
    fn models_from_meta_empty_when_all_locked() {
        let meta: acp::Meta = serde_json::json!({
            "modelState": {
                "availableModels": [
                    { "modelId": "a", "meta": { "requiresApiKey": true } }
                ]
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        assert!(models_from_meta(&Some(meta)).is_empty());
    }

    #[test]
    fn allows_model_only_for_configured_platforms() {
        let mut p = ConfiguredProviders::default();
        p.platforms.insert("openrouter".into());
        assert!(p.allows_model("openrouter/qwen-plus"));
        assert!(!p.allows_model("grok-4"));
        assert!(!p.allows_model("deepseek/v3"));
        p.xai = true;
        assert!(p.allows_model("grok-4"));
    }

    #[test]
    fn apply_auth_scope_maps_oauth_and_platform() {
        let mut p = ConfiguredProviders::default();
        apply_auth_scope(&mut p, "https://auth.x.ai::client");
        apply_auth_scope(&mut p, "oauth/openai-codex");
        apply_auth_scope(&mut p, "platform/ollama");
        assert!(p.xai);
        assert!(p.platforms.contains("openai-codex"));
        assert!(p.platforms.contains("ollama"));
    }
}
