//! Phase 1 native host: disk snapshots under `~/.grok/hypercore/` + real model stream.
//!
//! Credentials stay on the host (env / `auth.json`). They never enter core snapshots.
//!
//! [`open_model_stream_from_sampler_config`] is also used by the shell's
//! [`ShellHyperHost`](crate-level doc in xai-grok-shell) so stream bridging stays in one place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc;
use xai_grok_sampler::{
    ApiBackend, RequestId, SamplerConfig, SamplingChannel, SamplingClient, SamplingErrorInfo,
    SamplingErrorKind, SamplingEvent, stream_chat_completions, stream_messages, stream_responses,
};
use xai_grok_sampling_types::{
    ConversationItem, ConversationRequest, SamplingError, SentCredential, ToolCall, ToolSpec,
};
use xai_hyper_host::{
    HostError, HostToolCall, HostToolResult, HyperHost, ModelChunk, ModelStream,
    ModelStreamRequest, TerminalTurnRecord,
};

use crate::disk_store::HypercoreSessionStore;

const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_MODEL: &str = "grok-4";
const API_KEY_SCOPE: &str = "xai::api_key";

/// Configuration for [`NativeHost`].
#[derive(Debug, Clone)]
pub struct NativeHostConfig {
    /// Grok home directory (default: `~/.grok`).
    pub grok_home: PathBuf,
    /// Explicit API key (wins over env / auth.json when `Some`).
    pub api_key: Option<String>,
    /// API base URL (OpenAI-compatible `/v1`).
    pub base_url: String,
    /// Default model id.
    pub model: String,
    /// Wire backend.
    pub api_backend: ApiBackend,
}

impl Default for NativeHostConfig {
    fn default() -> Self {
        Self {
            grok_home: default_grok_home(),
            api_key: None,
            base_url: std::env::var("HYPERCORE_BASE_URL")
                .or_else(|_| std::env::var("XAI_BASE_URL"))
                .unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            model: std::env::var("HYPERCORE_MODEL")
                .or_else(|_| std::env::var("XAI_MODEL"))
                .unwrap_or_else(|_| DEFAULT_MODEL.into()),
            api_backend: parse_api_backend(
                &std::env::var("HYPERCORE_API_BACKEND").unwrap_or_default(),
            ),
        }
    }
}

impl NativeHostConfig {
    /// Build from environment + default home, resolving credentials lazily at stream open
    /// unless `api_key` is already set.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if cfg.api_key.is_none() {
            cfg.api_key = resolve_api_key(&cfg.grok_home);
        }
        cfg
    }
}

/// Disk + sampler host. Clones share the same open-stream counter.
#[derive(Debug, Clone)]
pub struct NativeHost {
    config: NativeHostConfig,
    store: HypercoreSessionStore,
    stream_opens: std::sync::Arc<AtomicU64>,
}

impl NativeHost {
    /// Create a host with the given config (does not require a key until stream open).
    pub fn new(config: NativeHostConfig) -> Self {
        let store = HypercoreSessionStore::new(config.grok_home.join("hypercore"));
        Self {
            config,
            store,
            stream_opens: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// Convenience: env + auth.json resolution for the key.
    pub fn from_env() -> Self {
        Self::new(NativeHostConfig::from_env())
    }

    /// Root directory for this host's sessions: `{grok_home}/hypercore`.
    pub fn hypercore_root(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    /// How many model streams have been opened (tests / demo stats).
    pub fn model_stream_opens(&self) -> u64 {
        self.stream_opens.load(Ordering::SeqCst)
    }

    /// Effective model id from config.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    fn resolved_api_key(&self) -> Result<String, HostError> {
        if let Some(k) = self
            .config
            .api_key
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Ok(k);
        }
        resolve_api_key(&self.config.grok_home).ok_or_else(|| {
            HostError::Message(
                "no API key: set XAI_API_KEY / HYPERCORE_API_KEY, or store a key in ~/.grok/auth.json"
                    .into(),
            )
        })
    }
}

struct ChannelModelStream {
    rx: mpsc::Receiver<Result<ModelChunk, HostError>>,
}

#[async_trait]
impl ModelStream for ChannelModelStream {
    async fn next_chunk(&mut self) -> Result<Option<ModelChunk>, HostError> {
        match self.rx.recv().await {
            Some(Ok(c)) => Ok(Some(c)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

/// Open a model stream from a fully-built [`SamplerConfig`] (credentials included).
///
/// Used by [`NativeHost`] and by the shell's `ShellHyperHost` so streaming logic
/// is not duplicated.
pub fn open_model_stream_from_sampler_config(
    mut sampler_cfg: SamplerConfig,
    req: ModelStreamRequest,
) -> Result<Box<dyn ModelStream>, HostError> {
    let model = if req.model.is_empty() {
        sampler_cfg.model.clone()
    } else {
        req.model.clone()
    };
    if !model.is_empty() {
        sampler_cfg.model = model.clone();
    }
    if matches!(sampler_cfg.api_backend, ApiBackend::CodexResponses) {
        sampler_cfg.responses_codex_dialect = true;
    }
    let api_backend = sampler_cfg.api_backend.clone();
    let client = SamplingClient::new(sampler_cfg)
        .map_err(|e| HostError::Transport(format!("SamplingClient::new: {e}")))?;

    let items: Vec<ConversationItem> = req
        .messages
        .iter()
        .filter_map(chat_message_to_conversation_item)
        .collect();
    let tool_specs: Vec<ToolSpec> = req
        .tools
        .iter()
        .map(|t| ToolSpec {
            name: t.name.clone(),
            description: if t.description.is_empty() {
                None
            } else {
                Some(t.description.clone())
            },
            parameters: t.input_schema.clone(),
        })
        .collect();
    let mut request = ConversationRequest::from_items(items)
        .with_model(model)
        .with_tools(tool_specs);
    if let Some(schema) = req.json_schema {
        request = request.with_json_schema(schema);
    }

    let (tx, rx) = mpsc::channel::<Result<ModelChunk, HostError>>(64);
    tokio::spawn(async move {
        if let Err(e) = drive_sampler_stream(client, api_backend, request, tx.clone()).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    Ok(Box::new(ChannelModelStream { rx }))
}

fn chat_message_to_conversation_item(m: &xai_hyper_host::ChatMessage) -> Option<ConversationItem> {
    match m.role.as_str() {
        "system" => Some(ConversationItem::system(m.content.clone())),
        "user" => Some(ConversationItem::user(m.content.clone())),
        "assistant" => {
            if m.tool_calls.is_empty() {
                Some(ConversationItem::assistant(m.content.clone()))
            } else {
                let calls: Vec<ToolCall> = m
                    .tool_calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: std::sync::Arc::<str>::from(tc.id.as_str()),
                        name: tc.name.clone(),
                        arguments: std::sync::Arc::<str>::from(tc.arguments.as_str()),
                    })
                    .collect();
                let mut item = ConversationItem::assistant_tool_calls(calls);
                if let ConversationItem::Assistant(ref mut a) = item
                    && !m.content.is_empty()
                {
                    a.content = std::sync::Arc::<str>::from(m.content.as_str());
                }
                Some(item)
            }
        }
        "tool" => {
            let call_id = m.tool_call_id.as_deref().unwrap_or("");
            Some(ConversationItem::tool_result(call_id, m.content.clone()))
        }
        _ => None,
    }
}

#[async_trait]
impl HyperHost for NativeHost {
    async fn open_model_stream(
        &self,
        req: ModelStreamRequest,
    ) -> Result<Box<dyn ModelStream>, HostError> {
        self.stream_opens.fetch_add(1, Ordering::SeqCst);
        let api_key = self.resolved_api_key()?;
        let model = if req.model.is_empty() {
            self.config.model.clone()
        } else {
            req.model.clone()
        };
        let api_backend = self.config.api_backend.clone();

        let mut sampler_cfg = SamplerConfig {
            api_key: Some(api_key),
            base_url: self.config.base_url.clone(),
            model: model.clone(),
            api_backend: api_backend.clone(),
            context_window: 131_072,
            max_retries: Some(2),
            client_version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        };
        if matches!(api_backend, ApiBackend::CodexResponses) {
            sampler_cfg.responses_codex_dialect = true;
        }

        open_model_stream_from_sampler_config(sampler_cfg, ModelStreamRequest { model, ..req })
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
        Err(HostError::Unsupported("tools"))
    }

    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

async fn drive_sampler_stream(
    client: SamplingClient,
    api_backend: ApiBackend,
    request: ConversationRequest,
    tx: mpsc::Sender<Result<ModelChunk, HostError>>,
) -> Result<(), HostError> {
    let request_id = RequestId::random();
    let idle = Duration::from_secs(300);

    // Open-time and event-path failures share [`sampling_error_to_host_error`]
    // so Auth status/credential never collapse to opaque Transport.
    match api_backend {
        ApiBackend::ChatCompletions => {
            let (raw, meta) = client
                .conversation_stream(request)
                .await
                .map_err(open_sampling_error_to_host_error)?;
            let stream = stream_chat_completions(raw, meta, request_id, idle);
            forward_sampling_events(stream, tx).await
        }
        ApiBackend::Responses | ApiBackend::CodexResponses => {
            let (raw, meta, doom) = client
                .conversation_stream_responses(request)
                .await
                .map_err(open_sampling_error_to_host_error)?;
            let stream = stream_responses(raw, meta, request_id, idle, doom);
            forward_sampling_events(stream, tx).await
        }
        ApiBackend::Messages => {
            let (raw, meta) = client
                .conversation_stream_messages(request)
                .await
                .map_err(open_sampling_error_to_host_error)?;
            let stream = stream_messages(raw, meta, request_id, idle);
            forward_sampling_events(stream, tx).await
        }
        other => Err(HostError::Message(format!(
            "unsupported api backend for hypercore Phase 1: {other:?}"
        ))),
    }
}

/// Map a Layer-1 open-time [`SamplingError`] the same way L2 event failures do.
fn open_sampling_error_to_host_error(err: SamplingError) -> HostError {
    sampling_error_to_host_error(SamplingErrorInfo::from(&err))
}

async fn forward_sampling_events<S>(
    stream: S,
    tx: mpsc::Sender<Result<ModelChunk, HostError>>,
) -> Result<(), HostError>
where
    S: futures::Stream<Item = SamplingEvent> + Send,
{
    tokio::pin!(stream);
    while let Some(ev) = stream.next().await {
        match ev {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Text,
                text,
                ..
            } if !text.is_empty() => {
                if tx.send(Ok(ModelChunk::TextDelta(text))).await.is_err() {
                    return Ok(());
                }
            }
            SamplingEvent::Completed { response, .. } => {
                // Emit complete tool calls from the aggregated response (P2).
                for tc in response.tool_calls() {
                    let chunk = ModelChunk::ToolCall {
                        id: tc.id.to_string(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.to_string(),
                    };
                    if tx.send(Ok(chunk)).await.is_err() {
                        return Ok(());
                    }
                }
                let stop = if !response.tool_calls().is_empty() {
                    Some("tool_calls".into())
                } else {
                    response
                        .raw_stop_reason
                        .clone()
                        .or_else(|| Some("end_turn".into()))
                };
                let _ = tx.send(Ok(ModelChunk::Done { stop_reason: stop })).await;
                return Ok(());
            }
            SamplingEvent::Failed { error, .. } => {
                // Do **not** `tx.send(Err)` here. The spawn wrapper around
                // `drive_sampler_stream` is the sole publisher of a terminal
                // channel Err, so a single Failed event yields exactly one
                // error to the consumer (then None on subsequent recv).
                return Err(sampling_error_to_host_error(error));
            }
            // ToolCallDelta: wait for Completed aggregation (avoids partial JSON).
            _ => {}
        }
    }
    let _ = tx
        .send(Ok(ModelChunk::Done {
            stop_reason: Some("end_turn".into()),
        }))
        .await;
    Ok(())
}

/// Map a sampler L1 open failure or L2 event failure into a typed [`HostError`]
/// so the shell Hypercore path can distinguish auth/401 from generic transport
/// errors and apply the same force-refresh + per-incident retry budget as the
/// legacy turn loop. **Open-time and event-path share this helper.**
fn sampling_error_to_host_error(error: SamplingErrorInfo) -> HostError {
    let is_auth = matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401);
    if is_auth {
        let credential = match error.credential {
            SentCredential::Sent => "sent",
            SentCredential::Missing => "missing",
            // Unknown + any future non-exhaustive variant: charge the budget.
            _ => "unknown",
        };
        HostError::Auth {
            // Auth kind from L1 often has no HTTP status on the wire path that
            // never got a response; still surface 401 so recovery is typed.
            status_code: error.status_code.or(Some(401)),
            message: error.message,
            credential: credential.to_owned(),
        }
    } else {
        HostError::Transport(error.message)
    }
}

#[cfg(test)]
mod sampling_error_map_tests {
    use super::*;
    use xai_grok_sampler::{SamplingErrorInfo, SamplingErrorKind};
    use xai_grok_sampling_types::SentCredential;

    fn info(
        kind: SamplingErrorKind,
        status: Option<u16>,
        credential: SentCredential,
    ) -> SamplingErrorInfo {
        SamplingErrorInfo {
            kind,
            status_code: status,
            message: "m".into(),
            is_retryable: true,
            retry_after_secs: None,
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential,
            error_code: None,
        }
    }

    #[test]
    fn auth_kind_maps_to_host_auth_with_credential() {
        let err = sampling_error_to_host_error(info(
            SamplingErrorKind::Auth,
            Some(401),
            SentCredential::Missing,
        ));
        match err {
            HostError::Auth {
                status_code,
                credential,
                ..
            } => {
                assert_eq!(status_code, Some(401));
                assert_eq!(credential, "missing");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn sent_credential_preserved() {
        let err =
            sampling_error_to_host_error(info(SamplingErrorKind::Auth, None, SentCredential::Sent));
        match err {
            HostError::Auth {
                status_code,
                credential,
                ..
            } => {
                assert_eq!(status_code, Some(401));
                assert_eq!(credential, "sent");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn non_auth_maps_to_transport() {
        let err = sampling_error_to_host_error(info(
            SamplingErrorKind::Http,
            Some(500),
            SentCredential::Unknown,
        ));
        assert!(matches!(err, HostError::Transport(_)));
        assert!(!err.is_auth_error());
    }

    /// Open-time L1 Auth must preserve credential through the shared helper
    /// used by all three backends (ChatCompletions / Responses / Messages).
    #[test]
    fn open_time_auth_missing_and_sent_preserve_credential() {
        for (cred, expect) in [
            (SentCredential::Missing, "missing"),
            (SentCredential::Sent, "sent"),
            (SentCredential::Unknown, "unknown"),
        ] {
            let l1 = SamplingError::Auth {
                message: "open rejected".into(),
                credential: cred,
            };
            let host = open_sampling_error_to_host_error(l1);
            match host {
                HostError::Auth {
                    status_code,
                    credential,
                    message,
                } => {
                    assert_eq!(status_code, Some(401), "Auth kind defaults status to 401");
                    assert_eq!(credential, expect);
                    assert!(message.contains("open rejected"));
                }
                other => panic!("expected typed Auth for {expect}, got {other:?}"),
            }
        }
    }

    /// Api 401 open failures (Responses / Messages often surface this way)
    /// also become typed Auth via the shared Info→Host path.
    #[test]
    fn open_time_api_401_is_typed_auth() {
        let host = sampling_error_to_host_error(info(
            SamplingErrorKind::Api,
            Some(401),
            SentCredential::Unknown,
        ));
        assert!(
            matches!(
                host,
                HostError::Auth {
                    status_code: Some(401),
                    ..
                }
            ),
            "got {host:?}"
        );
    }

    /// Event-path and open-path share the same mapping function identity.
    #[test]
    fn open_and_event_helpers_agree_on_auth() {
        let l1 = SamplingError::Auth {
            message: "x".into(),
            credential: SentCredential::Missing,
        };
        let via_open = open_sampling_error_to_host_error(l1);
        let via_event = sampling_error_to_host_error(info(
            SamplingErrorKind::Auth,
            None,
            SentCredential::Missing,
        ));
        match (via_open, via_event) {
            (
                HostError::Auth {
                    credential: c1,
                    status_code: s1,
                    ..
                },
                HostError::Auth {
                    credential: c2,
                    status_code: s2,
                    ..
                },
            ) => {
                assert_eq!(c1, c2);
                assert_eq!(s1, s2);
            }
            other => panic!("expected both Auth, got {other:?}"),
        }
    }

    /// Full bridge: one `SamplingEvent::Failed` through the same path as
    /// production (`forward` returns Err → spawn wrapper sends exactly one
    /// channel Err → subsequent recv is None). No double-terminal-error.
    #[tokio::test]
    async fn single_failed_event_yields_one_channel_err_then_none() {
        use futures::stream;
        use xai_grok_sampler::RequestId;
        use xai_grok_sampler::SamplingEvent;

        let (tx, mut rx) = mpsc::channel::<Result<ModelChunk, HostError>>(8);
        let failed = SamplingEvent::Failed {
            request_id: RequestId::random(),
            error: info(SamplingErrorKind::Auth, Some(401), SentCredential::Sent),
        };
        // Mirror open_model_stream_from_sampler_config spawn wrapper:
        // drive/forward returns Err → sole publisher of terminal Err.
        tokio::spawn(async move {
            let stream = stream::iter(vec![failed]);
            if let Err(e) = forward_sampling_events(stream, tx.clone()).await {
                let _ = tx.send(Err(e)).await;
            }
        });

        let first = rx.recv().await.expect("one terminal item");
        match first {
            Err(HostError::Auth {
                status_code: Some(401),
                credential,
                ..
            }) => assert_eq!(credential, "sent"),
            other => panic!("expected single typed Auth Err, got {other:?}"),
        }
        // Stream closed after the single terminal Err — no second Err, no Done.
        assert!(
            rx.recv().await.is_none(),
            "channel must end after the sole terminal Err"
        );
    }

    /// Regression: forward itself must not write Err (would double with spawn).
    #[tokio::test]
    async fn forward_failed_does_not_send_err_before_return() {
        use futures::stream;
        use xai_grok_sampler::RequestId;
        use xai_grok_sampler::SamplingEvent;

        let (tx, mut rx) = mpsc::channel::<Result<ModelChunk, HostError>>(8);
        let failed = SamplingEvent::Failed {
            request_id: RequestId::random(),
            error: info(SamplingErrorKind::Auth, Some(401), SentCredential::Missing),
        };
        let stream = stream::iter(vec![failed]);
        let result = forward_sampling_events(stream, tx).await;
        assert!(
            matches!(
                result,
                Err(HostError::Auth {
                    credential: ref c,
                    ..
                }) if c == "missing"
            ),
            "forward must return Err, got {result:?}"
        );
        // Channel must still be empty — no Err was sent by forward.
        assert!(
            rx.try_recv().is_err(),
            "forward must not tx.send(Err); spawn wrapper owns that"
        );
    }
}

/// Resolve API key: env vars first, then `auth.json` scopes.
pub fn resolve_api_key(grok_home: &Path) -> Option<String> {
    for var in ["XAI_API_KEY", "HYPERCORE_API_KEY", "GROK_CODE_XAI_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    read_key_from_auth_json(&grok_home.join("auth.json"))
}

fn read_key_from_auth_json(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).ok()?;
    if let Some(k) = map
        .get(API_KEY_SCOPE)
        .and_then(|e| e.get("key"))
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(k.to_string());
    }
    // Prefer OpenRouter platform key if present.
    if let Some(k) = map
        .get("platform/openrouter")
        .and_then(|e| e.get("key"))
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(k.to_string());
    }
    // Fall back to first xAI OIDC session token.
    for (scope, entry) in &map {
        if scope.starts_with("https://auth.x.ai")
            && let Some(k) = entry
                .get("key")
                .and_then(|k| k.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return Some(k.to_string());
        }
    }
    None
}

fn default_grok_home() -> PathBuf {
    if let Ok(p) = std::env::var("GROK_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
}

fn parse_api_backend(raw: &str) -> ApiBackend {
    match raw.trim().to_ascii_lowercase().as_str() {
        "responses" => ApiBackend::Responses,
        "codex_responses" | "codex-responses" => ApiBackend::CodexResponses,
        "messages" => ApiBackend::Messages,
        _ => ApiBackend::ChatCompletions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CoreConfig, HyperCore, TurnRequest};
    use xai_hyper_host::HyperHost;

    #[tokio::test]
    async fn native_disk_snapshot_and_terminal_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let host = NativeHost::new(NativeHostConfig {
            grok_home: dir.path().to_path_buf(),
            api_key: Some("test-key-not-used-for-replay".into()),
            base_url: "http://127.0.0.1:9".into(),
            model: "mock".into(),
            api_backend: ApiBackend::ChatCompletions,
        });

        // Manually commit a terminal + snapshot as if a turn finished.
        let session = "demo-sess";
        let snap = br#"{"schema_version":1,"session_id":"demo-sess","items":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}],"completed_turns":1,"model":"mock","extensions":{}}"#;
        let term = TerminalTurnRecord {
            turn_id: "t1".into(),
            request_text: "hi".into(),
            assistant_text: "hello".into(),
            stop_reason: Some("end_turn".into()),
        };
        host.commit_snapshot(session, snap, Some(&term))
            .await
            .unwrap();

        let mut core = HyperCore::restore_or_new(host.clone(), session, CoreConfig::default())
            .await
            .unwrap();
        assert_eq!(core.completed_turns(), 1);
        assert_eq!(core.items().len(), 2);

        let opens_before = host.model_stream_opens();
        let out = core
            .submit_turn(TurnRequest {
                turn_id: "t1".into(),
                text: "hi".into(),
                json_schema: None,
                tools: None,
            })
            .await
            .unwrap();
        assert!(out.replayed);
        assert_eq!(out.assistant_text, "hello");
        assert_eq!(host.model_stream_opens(), opens_before);
    }
}
