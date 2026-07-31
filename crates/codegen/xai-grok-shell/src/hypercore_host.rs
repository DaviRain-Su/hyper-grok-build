//! Shell-backed [`HyperHost`] for Hypercore integration.
//!
//! Holds a live [`SamplerConfig`] (credentials + model route from the session)
//! and persists core snapshots under `{grok_home}/hypercore/`.
//!
//! This does **not** yet replace `handle_prompt`; it is the host surface the
//! session can hand to [`xai_hyper_core::HyperCore`] on a feature-flagged path.
//!
//! See `docs/design-hypercore.md` § existing-shell integration.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use xai_grok_sampler::SamplerConfig;
use xai_hyper_core::disk_store::HypercoreSessionStore;
use xai_hyper_core::native::open_model_stream_from_sampler_config;
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, HyperHost, ModelStream, ModelStreamRequest,
    TerminalTurnRecord,
};

/// Shell implementation of [`HyperHost`].
///
/// Construct with a full session [`SamplerConfig`] (e.g. from
/// `reconstruct_full_config`). Call [`Self::replace_sampling_config`] when the
/// session model / credentials change.
#[derive(Clone)]
pub struct ShellHyperHost {
    sampling: Arc<RwLock<SamplerConfig>>,
    store: HypercoreSessionStore,
    stream_opens: Arc<AtomicU64>,
}

impl std::fmt::Debug for ShellHyperHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellHyperHost")
            .field("storage_root", &self.store.root())
            .field("stream_opens", &self.stream_opens.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ShellHyperHost {
    /// Create a host with an initial sampling config and storage root.
    ///
    /// `storage_root` is typically `{grok_home}/hypercore`.
    pub fn new(sampling: SamplerConfig, storage_root: impl Into<PathBuf>) -> Self {
        Self {
            sampling: Arc::new(RwLock::new(sampling)),
            store: HypercoreSessionStore::new(storage_root.into()),
            stream_opens: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create under `{grok_home}/hypercore`.
    pub fn under_grok_home(sampling: SamplerConfig, grok_home: impl AsRef<Path>) -> Self {
        Self::new(sampling, grok_home.as_ref().join("hypercore"))
    }

    /// Replace the sampling config (model switch, re-auth, etc.).
    pub fn replace_sampling_config(&self, sampling: SamplerConfig) {
        *self.sampling.write().expect("sampling lock") = sampling;
    }

    /// Snapshot of the current sampling config (includes secrets — host only).
    pub fn sampling_config(&self) -> SamplerConfig {
        self.sampling.read().expect("sampling lock").clone()
    }

    /// Storage root (`…/hypercore`).
    pub fn storage_root(&self) -> &Path {
        self.store.root()
    }

    /// Number of times a model stream was opened (not counting idempotent replays).
    pub fn model_stream_opens(&self) -> u64 {
        self.stream_opens.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HyperHost for ShellHyperHost {
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError> {
        let cfg = self.sampling_config();
        if cfg.api_key.is_none()
            && cfg.bearer_resolver.is_none()
            && !cfg
                .extra_headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            return Err(HostError::Message(
                "ShellHyperHost: SamplerConfig has no api_key, bearer_resolver, or Authorization header"
                    .into(),
            ));
        }
        self.stream_opens.fetch_add(1, Ordering::SeqCst);
        open_model_stream_from_sampler_config(cfg, req)
    }

    async fn commit_snapshot(
        &self,
        session_id: &str,
        snapshot: &[u8],
        terminal: Option<&TerminalTurnRecord>,
    ) -> Result<(), HostError> {
        self.store
            .commit_snapshot(session_id, snapshot, terminal)
            .await
    }

    async fn load_snapshot(&self, session_id: &str) -> Result<Option<Vec<u8>>, HostError> {
        self.store.load_snapshot(session_id).await
    }

    async fn load_terminal_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<TerminalTurnRecord>, HostError> {
        self.store.load_terminal_turn(session_id, turn_id).await
    }

    async fn invoke_tool(&self, _call: HostToolCall) -> Result<HostToolResult, HostError> {
        // Phase 1.5: tools stay on the legacy session path until wired.
        Err(HostError::Unsupported("shell tools via HyperHost"))
    }

    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_hyper_core::{CoreConfig, HyperCore, TurnRequest};
    use xai_hyper_host::HyperHost;

    fn test_sampler_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key-not-used-on-replay".into()),
            base_url: "http://127.0.0.1:9".into(),
            model: "test-model".into(),
            context_window: 128_000,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn shell_host_disk_restore_and_idempotent_turn() {
        let dir = tempfile::tempdir().unwrap();
        let host = ShellHyperHost::new(test_sampler_config(), dir.path().join("hypercore"));

        let session = "shell-sess-1";
        let snap = br#"{"schema_version":1,"session_id":"shell-sess-1","items":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello from shell host"}],"completed_turns":1,"model":"test-model","extensions":{}}"#;
        let term = TerminalTurnRecord {
            turn_id: "t1".into(),
            request_text: "hi".into(),
            assistant_text: "hello from shell host".into(),
            stop_reason: Some("end_turn".into()),
        };
        host.commit_snapshot(session, snap, Some(&term))
            .await
            .unwrap();

        let mut core = HyperCore::restore_or_new(host.clone(), session, CoreConfig::default())
            .await
            .unwrap();
        assert_eq!(core.completed_turns(), 1);

        let opens_before = host.model_stream_opens();
        let out = core
            .submit_turn(TurnRequest {
                turn_id: "t1".into(),
                text: "hi".into(),
            })
            .await
            .unwrap();
        assert!(out.replayed);
        assert_eq!(out.assistant_text, "hello from shell host");
        assert_eq!(host.model_stream_opens(), opens_before);
    }

    #[tokio::test]
    async fn replace_sampling_config_updates_model() {
        let dir = tempfile::tempdir().unwrap();
        let host = ShellHyperHost::new(test_sampler_config(), dir.path());
        assert_eq!(host.sampling_config().model, "test-model");
        let mut next = test_sampler_config();
        next.model = "other-model".into();
        host.replace_sampling_config(next);
        assert_eq!(host.sampling_config().model, "other-model");
    }

    #[tokio::test]
    async fn open_stream_rejects_empty_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_sampler_config();
        cfg.api_key = None;
        let host = ShellHyperHost::new(cfg, dir.path());
        let result = host
            .open_model_stream(ModelStreamRequest {
                session_id: "s".into(),
                turn_id: "t".into(),
                model: "m".into(),
                messages: vec![],
            })
            .await;
        let err = match result {
            Ok(_) => panic!("expected credential error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("no api_key"),
            "unexpected: {err}"
        );
        assert_eq!(host.model_stream_opens(), 0);
    }
}
