//! Hyper harness model catalog + reasoning ladder.
//!
//! `models()` is LIVE (D4): spawn `hyper agent stdio`, `initialize`, read
//! `modelState` from the init `_meta`, build `Vec<Model>`. Cached for the
//! process lifetime. The ACP handshake core (`init_meta_over`) lives in
//! `mod.rs` and is transport-injected so the e2e test can drive it without a
//! real binary; `live_models()` here spawns the real subprocess on a dedicated
//! `LocalSet` thread. When the init meta carries no `modelState`, `models()`
//! falls back to a static catalog so the picker still works.

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

/// Parse the live model catalog from the ACP `initialize` `_meta.modelState`.
///
/// Adapted from `xai-hyper-desktop/.../acp_backend.rs::models_from_meta` (which
/// returns desktop `ModelChoice`); here we build `comet_proto::Model` with the
/// reasoning ladder above. Returns `[]` when the meta carries no modelState.
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
        models.push(Model {
            id,
            label,
            description: None,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: Vec::new(),
        });
    }
    models
}

/// Live `models()` — cached for the process lifetime. Spawns `hyper agent
/// stdio` on a dedicated `LocalSet` thread (the ACP connection is `!Send`),
/// runs `initialize`, and parses `modelState`. Falls back to `static_fallback`
/// if the binary is missing or the meta has no modelState.
pub async fn live_models() -> Result<Vec<Model>, HarnessError> {
    static CACHE: tokio::sync::OnceCell<Vec<Model>> = tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_try_init(|| async {
            // Ensure (download if needed) before spawning the models probe.
            let exe = match super::ensure::ensure_hyper_bin().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "hyper ensure failed; using static model catalog");
                    return Ok(static_fallback());
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
                            None => return Ok(static_fallback()),
                        };
                        let incoming = match child.stdout.take() {
                            Some(s) => s.compat(),
                            None => return Ok(static_fallback()),
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
        })
        .await
        .map(|v| v.clone())
}

/// Static fallback model list — returned when the init meta carries no
/// `modelState` (or the live spawn failed before falling back at the caller).
/// `grok-4` / `grok-4-fast` are hyper defaults; the live query replaces these
/// with the actual configured providers.
pub(super) fn static_fallback() -> Vec<Model> {
    vec![
        Model {
            id: "grok-4".into(),
            label: "Grok 4".into(),
            description: None,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: Vec::new(),
        },
        Model {
            id: "grok-4-fast".into(),
            label: "Grok 4 Fast".into(),
            description: None,
            reasoning_levels: REASONING_LEVELS.to_vec(),
            options: Vec::new(),
        },
    ]
}