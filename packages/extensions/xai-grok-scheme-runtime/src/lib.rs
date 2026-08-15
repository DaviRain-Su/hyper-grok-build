//! Hyper scheme extension runtime — the Gambit/Gerbil live-image handler
//! backend on the session Extension Bus.
//!
//! Sits **beside** the WASM [`ExtensionRuntime`] as a fourth sequential
//! dispatch segment (shell hooks → client hooks → wasm → scheme), consuming
//! the same event/capability contract from `xai-grok-extension-api`:
//!
//! - one supervised image child process per session (lazy boot, respawn with
//!   a bounded budget, `quit → deadline → SIGKILL` teardown);
//! - every trusted plugin that declares `runtime.scheme` in `plugin.json`
//!   loads its policy script into that image under its own plugin namespace;
//! - plugin handlers are **tracked bindings**: `/live redefine` journals a
//!   new handler source (fsync-before-apply), `commit` promotes it after a
//!   clean-probe replay, `discard`/`recover` quarantine pending entries;
//! - every failure degrades per [`GateFailMode`] exactly like the WASM bus
//!   (default fail-open); a missing toolchain silently disables the feature.
//!
//! The image is **not a sandbox**: scripts run with user privileges under the
//! same trust bar as WASM guests (trusted + enabled plugins only). The child
//! environment is allowlisted (`PATH`/`HOME`/`TERM`), so ambient credentials
//! never enter the image.
//!
//! [`ExtensionRuntime`]: https://docs.rs/xai-grok-extension-runtime

mod frame;
mod image;
mod journal;
mod sexp;

pub use frame::MAX_FRAME_BYTES;
pub use image::{
    BOOT_TIMEOUT, ImageCommand, ImageError, KERNEL_SOURCE, PROTOCOL_VERSION, QUIT_TIMEOUT,
    ensure_kernel_cache, resolve_image_command,
};
pub use journal::{EffectiveRedefine, Journal, JournalEntry, JournalError, JournalStatus};
pub use sexp::{Sexp, SexpError};

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use image::ImageHandle;
use xai_grok_extension_api::{
    BeforeAgentStartIn, BeforeAgentStartOut, Capability, GateFailMode, PostToolIn, PreCompactIn,
    PreToolIn, PreToolOut, SchemeCommandDescriptor, SchemeSpec, SchemeToolDescriptor, StopIn,
    StopOut, is_valid_guest_tool_name, timeouts,
};

/// Wall-clock budget for one registered command / tool invocation (the image
/// is pure compute in v1; generous compared to the 2s gate budget).
const INVOKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Wire event names (kernel dispatch symbols).
pub const EVENT_NAMES: &[&str] = &[
    "session-start",
    "user-prompt-submit",
    "before-agent-start",
    "before-model",
    "pre-tool-use",
    "post-tool-use",
    "notification",
    "subagent-stop",
    "stop",
    "pre-compact",
    "session-end",
];

/// Respawn budget per session (reset by `/live recover`).
const MAX_RESPAWNS: u32 = 3;

/// Cap for one plugin policy script.
const MAX_PLUGIN_SOURCE_BYTES: usize = 256 * 1024;

/// Runtime construction parameters (host-owned paths; the crate never
/// discovers a home directory by itself).
#[derive(Debug, Clone)]
pub struct SchemeRuntimeConfig {
    /// Live state root (journal + kernel cache), e.g. `~/.grok/live`.
    pub state_dir: PathBuf,
    /// Prebuilt image binary candidates, e.g. `~/.grok/bin/hyper-scheme-image`.
    pub prebuilt_candidates: Vec<PathBuf>,
    /// Allow falling back to `gxi` / `gsi` found on PATH. Disable for
    /// deterministic tests or via host config.
    pub allow_path_discovery: bool,
}

impl SchemeRuntimeConfig {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            prebuilt_candidates: Vec::new(),
            allow_path_discovery: true,
        }
    }
}

/// Outcome of one plugin handler invocation (mirror of the WASM
/// `GuestCallResult` for UI / telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemeCallResult {
    Ok {
        plugin: String,
        /// Rendered kernel reply, e.g. `(deny "reason")`.
        reply: String,
    },
    /// Plugin has no handler registered for this event.
    NoHandler { plugin: String },
    /// Handler raised or returned an unsupported value inside the image.
    HandlerError { plugin: String, error: String },
    SkippedCapability {
        plugin: String,
        capability: Capability,
    },
    /// Transport/protocol failure — the image was killed and will respawn
    /// (within budget) on the next dispatch.
    Failed { plugin: String, error: String },
    Timeout { plugin: String, limit: Duration },
    /// No image available (toolchain missing / respawn budget exhausted).
    Unavailable { plugin: String },
}

#[derive(Debug)]
pub struct SchemePreToolDispatch {
    pub decision: PreToolOut,
    /// Plugin name that produced a deny decision (for UI / telemetry).
    pub denied_by: Option<String>,
    pub results: Vec<SchemeCallResult>,
}

#[derive(Debug)]
pub struct SchemeStopDispatch {
    pub decision: StopOut,
    pub results: Vec<SchemeCallResult>,
}

#[derive(Debug)]
pub struct SchemeInjectDispatch {
    pub out: BeforeAgentStartOut,
    pub results: Vec<SchemeCallResult>,
}

impl SchemeInjectDispatch {
    pub fn has_injection(&self) -> bool {
        self.out.inject_context.is_some() || self.out.append_system.is_some()
    }
}

/// `/live status` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStatus {
    /// Whether an image is currently running.
    pub image_running: bool,
    /// Kernel self-identification from the handshake (when running).
    pub kernel_version: Option<String>,
    /// Loaded plugins: (name, load_failed).
    pub plugins: Vec<(String, bool)>,
    pub journal: JournalStatus,
    pub respawns: u32,
    pub image_command: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("scheme runtime unavailable (no image; toolchain missing or respawn budget exhausted)")]
    Unavailable,
    #[error("unknown plugin `{0}`")]
    NoSuchPlugin(String),
    #[error("unknown event `{0}` (expected one of: session-start, user-prompt-submit, before-agent-start, before-model, pre-tool-use, post-tool-use, notification, subagent-stop, stop, pre-compact, session-end)")]
    BadEvent(String),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("image error: {0}")]
    Image(String),
    #[error("commit rejected; pending redefines quarantined: {0}")]
    CommitRejected(String),
}

/// Operational counters (shared across clones).
#[derive(Debug, Default)]
pub struct SchemeMetrics {
    pub loads_ok: AtomicU64,
    pub loads_failed: AtomicU64,
    pub calls_ok: AtomicU64,
    pub calls_failed: AtomicU64,
    pub calls_timeout: AtomicU64,
    pub pre_tool_denies: AtomicU64,
    pub stop_blocks: AtomicU64,
    pub respawns: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchemeMetricsSnapshot {
    pub loads_ok: u64,
    pub loads_failed: u64,
    pub calls_ok: u64,
    pub calls_failed: u64,
    pub calls_timeout: u64,
    pub pre_tool_denies: u64,
    pub stop_blocks: u64,
    pub respawns: u64,
}

impl SchemeMetrics {
    fn snapshot(&self) -> SchemeMetricsSnapshot {
        SchemeMetricsSnapshot {
            loads_ok: self.loads_ok.load(Ordering::Relaxed),
            loads_failed: self.loads_failed.load(Ordering::Relaxed),
            calls_ok: self.calls_ok.load(Ordering::Relaxed),
            calls_failed: self.calls_failed.load(Ordering::Relaxed),
            calls_timeout: self.calls_timeout.load(Ordering::Relaxed),
            pre_tool_denies: self.pre_tool_denies.load(Ordering::Relaxed),
            stop_blocks: self.stop_blocks.load(Ordering::Relaxed),
            respawns: self.respawns.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Display for SchemeMetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "loads_ok={} loads_failed={} calls_ok={} calls_failed={} calls_timeout={} \
             pre_tool_denies={} stop_blocks={} respawns={}",
            self.loads_ok,
            self.loads_failed,
            self.calls_ok,
            self.calls_failed,
            self.calls_timeout,
            self.pre_tool_denies,
            self.stop_blocks,
            self.respawns,
        )
    }
}

/// Sync-readable plugin metadata (dispatch gating without the image lock).
#[derive(Debug, Clone)]
struct PluginMeta {
    name: String,
    capabilities: Vec<Capability>,
    gate_fail: Option<GateFailMode>,
}

struct PluginEntry {
    meta: PluginMeta,
    source: String,
    load_failed: bool,
}

struct Inner {
    config: SchemeRuntimeConfig,
    plugins: Vec<PluginEntry>,
    image: Option<ImageHandle>,
    image_command: Option<ImageCommand>,
    respawns: u32,
    unavailable_logged: bool,
}

/// Per-session scheme extension runtime. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct SchemeRuntime {
    inner: Arc<tokio::sync::Mutex<Inner>>,
    /// Sync mirror of loaded plugin metadata for lock-free gating checks.
    meta: Arc<std::sync::RwLock<Vec<PluginMeta>>>,
    gate_fail: GateFailMode,
    metrics: Arc<SchemeMetrics>,
}

enum CallOutcome {
    Reply(Sexp),
    Failed(String),
    Timeout(Duration),
    Unavailable,
}

impl SchemeRuntime {
    pub fn new(config: SchemeRuntimeConfig) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(Inner {
                config,
                plugins: Vec::new(),
                image: None,
                image_command: None,
                respawns: 0,
                unavailable_logged: false,
            })),
            meta: Arc::new(std::sync::RwLock::new(Vec::new())),
            gate_fail: GateFailMode::from_env(),
            metrics: Arc::new(SchemeMetrics::default()),
        }
    }

    pub fn with_gate_fail(mut self, mode: GateFailMode) -> Self {
        self.gate_fail = mode;
        self
    }

    pub fn metrics(&self) -> SchemeMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn is_empty(&self) -> bool {
        self.meta.read().map(|m| m.is_empty()).unwrap_or(true)
    }

    pub fn len(&self) -> usize {
        self.meta.read().map(|m| m.len()).unwrap_or(0)
    }

    pub fn names(&self) -> Vec<String> {
        self.meta
            .read()
            .map(|m| m.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn has_capability(&self, cap: Capability) -> bool {
        self.meta
            .read()
            .map(|m| m.iter().any(|p| p.capabilities.contains(&cap)))
            .unwrap_or(false)
    }

    /// Replace contents from trusted specs. Reads each script from disk;
    /// untrusted / unreadable / oversized specs are skipped with a warning.
    /// The image (re)boots lazily on the next dispatch.
    pub async fn rebuild_from_specs(&self, specs: impl IntoIterator<Item = SchemeSpec>) {
        let mut plugins = Vec::new();
        #[cfg(feature = "scheme")]
        for spec in specs {
            if !spec.may_load() {
                tracing::warn!(plugin = %spec.name, "untrusted scheme spec skipped");
                self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let source = match std::fs::read_to_string(&spec.scheme_path) {
                Ok(s) if s.len() <= MAX_PLUGIN_SOURCE_BYTES => s,
                Ok(s) => {
                    tracing::warn!(
                        plugin = %spec.name,
                        bytes = s.len(),
                        "scheme script exceeds {MAX_PLUGIN_SOURCE_BYTES} bytes; skipping"
                    );
                    self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %spec.name,
                        path = %spec.scheme_path.display(),
                        error = %e,
                        "failed to read scheme script; skipping"
                    );
                    self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            plugins.push(PluginEntry {
                meta: PluginMeta {
                    name: spec.name.clone(),
                    capabilities: spec.capabilities.clone(),
                    gate_fail: spec.gate_fail,
                },
                source,
                load_failed: false,
            });
        }
        #[cfg(not(feature = "scheme"))]
        {
            let _ = specs;
        }

        let mut inner = self.inner.lock().await;
        if let Some(image) = inner.image.take() {
            image.shutdown().await;
        }
        inner.respawns = 0;
        inner.unavailable_logged = false;
        inner.image_command = None;
        if let Ok(mut meta) = self.meta.write() {
            *meta = plugins.iter().map(|p| p.meta.clone()).collect();
        }
        inner.plugins = plugins;
    }

    /// Graceful teardown (session end / plugin reload).
    pub async fn shutdown_async(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(image) = inner.image.take() {
            image.shutdown().await;
        }
        inner.plugins.clear();
        if let Ok(mut meta) = self.meta.write() {
            meta.clear();
        }
    }

    fn effective_gate_fail(&self, plugin_gate_fail: Option<GateFailMode>) -> GateFailMode {
        plugin_gate_fail.unwrap_or(self.gate_fail)
    }

    fn plugins_with(&self, cap: Option<Capability>) -> Vec<PluginMeta> {
        self.meta
            .read()
            .map(|m| {
                m.iter()
                    .filter(|p| cap.is_none_or(|c| p.capabilities.contains(&c)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    // --- dispatch API (mirrors ExtensionRuntime) ---

    pub async fn dispatch_session_start(&self) -> Vec<SchemeCallResult> {
        self.dispatch_observe("session-start", Vec::new(), timeouts::OBSERVE)
            .await
    }

    pub async fn dispatch_session_end(&self) -> Vec<SchemeCallResult> {
        self.dispatch_observe("session-end", Vec::new(), timeouts::OBSERVE)
            .await
    }

    pub async fn dispatch_post_tool_use(&self, input: &PostToolIn) -> Vec<SchemeCallResult> {
        let input = input.clone().capped();
        let ctx = vec![
            Sexp::kv("tool-name", Sexp::str(input.tool_name)),
            Sexp::kv("success", Sexp::Bool(input.success)),
            Sexp::kv("tool-input", Sexp::str(input.tool_input_json)),
            Sexp::kv("tool-result", Sexp::str(input.tool_result_preview)),
        ];
        self.dispatch_observe("post-tool-use", ctx, timeouts::OBSERVE)
            .await
    }

    pub async fn dispatch_pre_compact(&self, input: &PreCompactIn) -> Vec<SchemeCallResult> {
        let ctx = vec![Sexp::kv("reason", Sexp::str(input.reason.clone()))];
        self.dispatch_observe("pre-compact", ctx, timeouts::OBSERVE)
            .await
    }

    /// Observe: the user submitted a prompt (fail-open, no gating).
    pub async fn dispatch_user_prompt_submit(&self, prompt: &str) -> Vec<SchemeCallResult> {
        let prompt = cap_utf8(prompt, xai_grok_extension_api::MAX_TOOL_PAYLOAD_BYTES);
        let ctx = vec![Sexp::kv("prompt", Sexp::str(prompt))];
        self.dispatch_observe("user-prompt-submit", ctx, timeouts::OBSERVE)
            .await
    }

    /// Observe: a host notification fired (fail-open).
    pub async fn dispatch_notification(&self, message: &str) -> Vec<SchemeCallResult> {
        let message = cap_utf8(message, xai_grok_extension_api::MAX_INJECT_BYTES);
        let ctx = vec![Sexp::kv("message", Sexp::str(message))];
        self.dispatch_observe("notification", ctx, timeouts::OBSERVE)
            .await
    }

    /// Observe: a subagent finished (fail-open).
    pub async fn dispatch_subagent_stop(&self, agent: &str) -> Vec<SchemeCallResult> {
        let ctx = vec![Sexp::kv("agent", Sexp::str(agent.to_string()))];
        self.dispatch_observe("subagent-stop", ctx, timeouts::OBSERVE)
            .await
    }

    async fn dispatch_observe(
        &self,
        event: &str,
        ctx: Vec<Sexp>,
        timeout: Duration,
    ) -> Vec<SchemeCallResult> {
        let plugins = self.plugins_with(None);
        if plugins.is_empty() {
            return Vec::new();
        }
        let mut inner = self.inner.lock().await;
        let mut out = Vec::new();
        for plugin in &plugins {
            let r = self
                .call_plugin(&mut inner, event, &plugin.name, ctx.clone(), timeout)
                .await;
            match self.outcome_to_result(&plugin.name, r) {
                // Mirror wasm: missing handlers are not reported for observes.
                SchemeCallResult::NoHandler { .. } => {}
                other => out.push(other),
            }
        }
        out
    }

    /// Before agent start: merge inject/append from plugins with
    /// [`Capability::BeforeAgentInject`].
    pub async fn dispatch_before_agent_start(
        &self,
        input: &BeforeAgentStartIn,
    ) -> SchemeInjectDispatch {
        self.dispatch_inject(
            "before-agent-start",
            Capability::BeforeAgentInject,
            input,
            timeouts::BEFORE_AGENT,
        )
        .await
    }

    /// Before each model round: inject only, no history rewrite.
    pub async fn dispatch_before_model(&self, input: &BeforeAgentStartIn) -> SchemeInjectDispatch {
        self.dispatch_inject(
            "before-model",
            Capability::BeforeModelInject,
            input,
            timeouts::BEFORE_AGENT,
        )
        .await
    }

    async fn dispatch_inject(
        &self,
        event: &str,
        cap: Capability,
        input: &BeforeAgentStartIn,
        timeout: Duration,
    ) -> SchemeInjectDispatch {
        let plugins = self.plugins_with(None);
        let mut merged = BeforeAgentStartOut::default();
        let mut results = Vec::new();
        if plugins.is_empty() {
            return SchemeInjectDispatch {
                out: merged,
                results,
            };
        }
        let ctx = vec![Sexp::kv("prompt", Sexp::str(input.prompt.clone()))];
        let mut inner = self.inner.lock().await;
        for plugin in &plugins {
            if !plugin.capabilities.contains(&cap) {
                results.push(SchemeCallResult::SkippedCapability {
                    plugin: plugin.name.clone(),
                    capability: cap,
                });
                continue;
            }
            let outcome = self
                .call_plugin(&mut inner, event, &plugin.name, ctx.clone(), timeout)
                .await;
            if let CallOutcome::Reply(ref reply) = outcome
                && reply.head_sym() == Some("inject")
            {
                let piece = BeforeAgentStartOut {
                    inject_context: reply
                        .arg(0)
                        .and_then(Sexp::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("[scheme:{}] {s}", plugin.name)),
                    append_system: reply
                        .arg(1)
                        .and_then(Sexp::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("[scheme:{}] {s}", plugin.name)),
                };
                merged = merged.merge_append(piece);
            }
            results.push(self.outcome_to_result(&plugin.name, outcome));
        }
        SchemeInjectDispatch {
            out: merged.truncated(),
            results,
        }
    }

    /// Pre-tool gate: first deny wins among plugins with
    /// [`Capability::PreToolGate`]. Trap/timeout follows the plugin's
    /// effective [`GateFailMode`].
    pub async fn dispatch_pre_tool_use(&self, input: &PreToolIn) -> SchemePreToolDispatch {
        let plugins = self.plugins_with(None);
        let mut results = Vec::new();
        if plugins.is_empty() {
            return SchemePreToolDispatch {
                decision: PreToolOut::Allow,
                denied_by: None,
                results,
            };
        }
        let input = input.clone().capped();
        let ctx = vec![
            Sexp::kv("tool-name", Sexp::str(input.tool_name.clone())),
            Sexp::kv("tool-input", Sexp::str(input.tool_input_json.clone())),
        ];
        let mut inner = self.inner.lock().await;
        for plugin in &plugins {
            if !plugin.capabilities.contains(&Capability::PreToolGate) {
                results.push(SchemeCallResult::SkippedCapability {
                    plugin: plugin.name.clone(),
                    capability: Capability::PreToolGate,
                });
                continue;
            }
            let outcome = self
                .call_plugin(&mut inner, "pre-tool-use", &plugin.name, ctx.clone(), timeouts::GATE)
                .await;
            let deny = gate_deny_reason(
                &outcome,
                self.effective_gate_fail(plugin.gate_fail),
                &plugin.name,
                &input.tool_name,
            );
            results.push(self.outcome_to_result(&plugin.name, outcome));
            if let Some(reason) = deny {
                self.metrics.pre_tool_denies.fetch_add(1, Ordering::Relaxed);
                return SchemePreToolDispatch {
                    decision: PreToolOut::Deny { reason },
                    denied_by: Some(plugin.name.clone()),
                    results,
                };
            }
        }
        SchemePreToolDispatch {
            decision: PreToolOut::Allow,
            denied_by: None,
            results,
        }
    }

    /// Stop gate: first block wins among plugins with [`Capability::StopGate`].
    pub async fn dispatch_stop(&self, input: &StopIn) -> SchemeStopDispatch {
        let plugins = self.plugins_with(None);
        let mut results = Vec::new();
        if plugins.is_empty() {
            return SchemeStopDispatch {
                decision: StopOut::Continue,
                results,
            };
        }
        let ctx = vec![Sexp::kv(
            "stop-hook-active",
            Sexp::Bool(input.stop_hook_active),
        )];
        let mut inner = self.inner.lock().await;
        for plugin in &plugins {
            if !plugin.capabilities.contains(&Capability::StopGate) {
                results.push(SchemeCallResult::SkippedCapability {
                    plugin: plugin.name.clone(),
                    capability: Capability::StopGate,
                });
                continue;
            }
            let outcome = self
                .call_plugin(&mut inner, "stop", &plugin.name, ctx.clone(), timeouts::GATE)
                .await;
            let block = stop_block_reason(
                &outcome,
                self.effective_gate_fail(plugin.gate_fail),
                &plugin.name,
            );
            results.push(self.outcome_to_result(&plugin.name, outcome));
            if let Some(reason) = block {
                self.metrics.stop_blocks.fetch_add(1, Ordering::Relaxed);
                return SchemeStopDispatch {
                    decision: StopOut::Block { reason },
                    results,
                };
            }
        }
        SchemeStopDispatch {
            decision: StopOut::Continue,
            results,
        }
    }

    // --- registered commands / tools (RegisterCommand / RegisterTool) ---

    /// Slash commands registered by plugins with [`Capability::RegisterCommand`]
    /// (`register-command!` in the plugin script). Boots the image if needed.
    pub async fn collect_registered_commands(&self) -> Vec<SchemeCommandDescriptor> {
        let allowed: Vec<String> = self
            .plugins_with(Some(Capability::RegisterCommand))
            .into_iter()
            .map(|p| p.name)
            .collect();
        if allowed.is_empty() {
            return Vec::new();
        }
        let mut inner = self.inner.lock().await;
        if !self.ensure_image(&mut inner).await {
            return Vec::new();
        }
        let req = Sexp::list(vec![Sexp::sym("list-commands")]);
        let CallOutcome::Reply(reply) = self.image_request(&mut inner, &req, timeouts::INIT).await
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if reply.head_sym() == Some("commands")
            && let Some(Sexp::List(items)) = reply.arg(0)
        {
            for item in items {
                let (Some(plugin), Some(name), Some(desc)) = (
                    item.nth(0).and_then(Sexp::as_str),
                    item.nth(1).and_then(Sexp::as_str),
                    item.nth(2).and_then(Sexp::as_str),
                ) else {
                    continue;
                };
                if !allowed.iter().any(|p| p == plugin) {
                    tracing::warn!(
                        target: "scheme_extension",
                        plugin = %plugin,
                        command = %name,
                        "scheme command registered without register_command capability; skipped"
                    );
                    continue;
                }
                if !is_valid_guest_tool_name(name) {
                    tracing::warn!(
                        target: "scheme_extension",
                        plugin = %plugin,
                        command = %name,
                        "invalid scheme command name; skipped"
                    );
                    continue;
                }
                out.push(SchemeCommandDescriptor {
                    extension: plugin.to_string(),
                    name: name.to_string(),
                    description: desc.to_string(),
                });
            }
        }
        out.sort_by(|a, b| (&a.extension, &a.name).cmp(&(&b.extension, &b.name)));
        out
    }

    /// Invoke a registered slash command. `Err` carries a display string.
    pub async fn invoke_registered_command(
        &self,
        extension: &str,
        name: &str,
        args: &str,
    ) -> Result<String, String> {
        self.invoke_registered(
            "invoke-command",
            Capability::RegisterCommand,
            extension,
            name,
            args,
        )
        .await
    }

    /// Model-visible tools registered by plugins with [`Capability::RegisterTool`]
    /// (`register-tool!` in the plugin script). Boots the image if needed.
    pub async fn collect_registered_tools(&self) -> Vec<SchemeToolDescriptor> {
        let allowed: Vec<String> = self
            .plugins_with(Some(Capability::RegisterTool))
            .into_iter()
            .map(|p| p.name)
            .collect();
        if allowed.is_empty() {
            return Vec::new();
        }
        let mut inner = self.inner.lock().await;
        if !self.ensure_image(&mut inner).await {
            return Vec::new();
        }
        let req = Sexp::list(vec![Sexp::sym("list-tools")]);
        let CallOutcome::Reply(reply) = self.image_request(&mut inner, &req, timeouts::INIT).await
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if reply.head_sym() == Some("tools")
            && let Some(Sexp::List(items)) = reply.arg(0)
        {
            for item in items {
                let (Some(plugin), Some(name), Some(desc), Some(schema)) = (
                    item.nth(0).and_then(Sexp::as_str),
                    item.nth(1).and_then(Sexp::as_str),
                    item.nth(2).and_then(Sexp::as_str),
                    item.nth(3).and_then(Sexp::as_str),
                ) else {
                    continue;
                };
                if !allowed.iter().any(|p| p == plugin) {
                    tracing::warn!(
                        target: "scheme_extension",
                        plugin = %plugin,
                        tool = %name,
                        "scheme tool registered without register_tool capability; skipped"
                    );
                    continue;
                }
                if !is_valid_guest_tool_name(name)
                    || !xai_grok_extension_api::is_valid_tool_schema_json(schema)
                {
                    tracing::warn!(
                        target: "scheme_extension",
                        plugin = %plugin,
                        tool = %name,
                        "invalid scheme tool name or schema; skipped"
                    );
                    continue;
                }
                out.push(SchemeToolDescriptor {
                    extension: plugin.to_string(),
                    name: name.to_string(),
                    description: desc.to_string(),
                    input_schema_json: schema.to_string(),
                });
            }
        }
        out.sort_by(|a, b| (&a.extension, &a.name).cmp(&(&b.extension, &b.name)));
        out
    }

    /// Invoke a registered tool with a JSON argument string.
    pub async fn invoke_registered_tool(
        &self,
        extension: &str,
        name: &str,
        input_json: &str,
    ) -> Result<String, String> {
        self.invoke_registered(
            "invoke-tool",
            Capability::RegisterTool,
            extension,
            name,
            input_json,
        )
        .await
    }

    async fn invoke_registered(
        &self,
        op: &str,
        cap: Capability,
        extension: &str,
        name: &str,
        arg: &str,
    ) -> Result<String, String> {
        let allowed = self
            .plugins_with(Some(cap))
            .into_iter()
            .any(|p| p.name == extension);
        if !allowed {
            return Err(format!(
                "scheme extension `{extension}` is not loaded or lacks the {} capability",
                cap.as_str()
            ));
        }
        let arg = cap_utf8(arg, xai_grok_extension_api::MAX_TOOL_PAYLOAD_BYTES);
        let mut inner = self.inner.lock().await;
        if !self.ensure_image(&mut inner).await {
            return Err("scheme runtime unavailable".to_string());
        }
        let req = Sexp::list(vec![
            Sexp::sym(op),
            Sexp::str(extension),
            Sexp::str(name),
            Sexp::str(arg),
        ]);
        match self.image_request(&mut inner, &req, INVOKE_TIMEOUT).await {
            CallOutcome::Reply(r) if r.head_sym() == Some("ok") => Ok(r
                .arg(0)
                .and_then(Sexp::as_str)
                .unwrap_or_default()
                .to_string()),
            CallOutcome::Reply(r) => Err(r
                .arg(0)
                .and_then(Sexp::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| r.render())),
            CallOutcome::Failed(e) => Err(e),
            CallOutcome::Timeout(d) => Err(format!("timed out after {d:?}")),
            CallOutcome::Unavailable => Err("scheme runtime unavailable".to_string()),
        }
    }

    // --- live self-modification API (`/live …`) ---

    pub async fn live_status(&self) -> LiveStatus {
        let inner = self.inner.lock().await;
        let journal = Journal::new(&inner.config.state_dir)
            .load_effective()
            .map(|(_, s)| s)
            .unwrap_or_default();
        LiveStatus {
            image_running: inner.image.is_some(),
            kernel_version: inner.image.as_ref().map(|i| i.kernel_version.clone()),
            plugins: inner
                .plugins
                .iter()
                .map(|p| (p.meta.name.clone(), p.load_failed))
                .collect(),
            journal,
            respawns: inner.respawns,
            image_command: inner.image_command.as_ref().map(|c| c.describe()),
        }
    }

    /// Journal (fsync) then apply a handler redefinition to the running image.
    /// The entry stays **pending** until `live_commit`.
    pub async fn live_redefine(
        &self,
        plugin: &str,
        event: &str,
        source: &str,
    ) -> Result<(), LiveError> {
        if !EVENT_NAMES.contains(&event) {
            return Err(LiveError::BadEvent(event.to_string()));
        }
        let mut inner = self.inner.lock().await;
        if !inner.plugins.iter().any(|p| p.meta.name == plugin) {
            return Err(LiveError::NoSuchPlugin(plugin.to_string()));
        }
        // fsync before apply: a crash after this line replays the redefine.
        Journal::new(&inner.config.state_dir).append(&JournalEntry::redefine(plugin, event, source))?;
        if !self.ensure_image(&mut inner).await {
            return Err(LiveError::Unavailable);
        }
        let req = Sexp::list(vec![
            Sexp::sym("redefine"),
            Sexp::str(plugin),
            Sexp::sym(event),
            Sexp::str(source),
        ]);
        match self.image_request(&mut inner, &req, timeouts::GATE).await {
            CallOutcome::Reply(r) if r.head_sym() == Some("ok") => Ok(()),
            CallOutcome::Reply(r) => Err(LiveError::Image(format!(
                "redefine rejected (journaled as pending; commit will quarantine): {r}"
            ))),
            CallOutcome::Failed(e) => Err(LiveError::Image(e)),
            CallOutcome::Timeout(d) => Err(LiveError::Image(format!("timed out after {d:?}"))),
            CallOutcome::Unavailable => Err(LiveError::Unavailable),
        }
    }

    /// Commit = clean-probe replay: a **fresh** image process must load every
    /// plugin and apply every effective redefine (committed + pending). On
    /// success pending entries are promoted; on failure they are quarantined
    /// and the live image restarts from the committed state.
    pub async fn live_commit(&self) -> Result<JournalStatus, LiveError> {
        let mut inner = self.inner.lock().await;
        let journal = Journal::new(&inner.config.state_dir);
        let (effective, status) = journal.load_effective()?;
        if status.pending == 0 {
            return Ok(status);
        }
        let probe = self.run_commit_probe(&inner, &effective).await;
        match probe {
            Ok(()) => {
                journal.append(&JournalEntry::commit())?;
                Ok(journal.load_effective()?.1)
            }
            Err(detail) => {
                journal.append(&JournalEntry::quarantine())?;
                // Restart the live image so it drops the quarantined state.
                if let Some(image) = inner.image.take() {
                    image.shutdown().await;
                }
                Err(LiveError::CommitRejected(detail))
            }
        }
    }

    /// Quarantine all pending redefines and restart from committed state.
    pub async fn live_discard(&self) -> Result<JournalStatus, LiveError> {
        let mut inner = self.inner.lock().await;
        let journal = Journal::new(&inner.config.state_dir);
        journal.append(&JournalEntry::quarantine())?;
        if let Some(image) = inner.image.take() {
            image.shutdown().await;
        }
        Ok(journal.load_effective()?.1)
    }

    /// `live_discard` + reset the respawn budget (recovery affordance).
    pub async fn live_recover(&self) -> Result<JournalStatus, LiveError> {
        let status = {
            let mut inner = self.inner.lock().await;
            let journal = Journal::new(&inner.config.state_dir);
            journal.append(&JournalEntry::quarantine())?;
            if let Some(image) = inner.image.take() {
                image.shutdown().await;
            }
            inner.respawns = 0;
            inner.unavailable_logged = false;
            journal.load_effective()?.1
        };
        Ok(status)
    }

    /// Host-driven eval inside the image (user CLI only; never model-visible).
    pub async fn live_eval(&self, source: &str) -> Result<String, LiveError> {
        let mut inner = self.inner.lock().await;
        if !self.ensure_image(&mut inner).await {
            return Err(LiveError::Unavailable);
        }
        let req = Sexp::list(vec![Sexp::sym("eval"), Sexp::str(source)]);
        match self.image_request(&mut inner, &req, timeouts::GATE).await {
            CallOutcome::Reply(r) if r.head_sym() == Some("ok") => {
                Ok(r.arg(0).and_then(Sexp::as_str).unwrap_or_default().to_string())
            }
            CallOutcome::Reply(r) => Err(LiveError::Image(r.render())),
            CallOutcome::Failed(e) => Err(LiveError::Image(e)),
            CallOutcome::Timeout(d) => Err(LiveError::Image(format!("timed out after {d:?}"))),
            CallOutcome::Unavailable => Err(LiveError::Unavailable),
        }
    }

    // --- internals ---

    /// Boot the image if needed. Returns whether an image is available.
    async fn ensure_image(&self, inner: &mut Inner) -> bool {
        #[cfg(not(feature = "scheme"))]
        {
            let _ = inner;
            return false;
        }
        #[cfg(feature = "scheme")]
        {
            if inner.image.is_some() {
                return true;
            }
            if inner.plugins.is_empty() || inner.respawns >= MAX_RESPAWNS {
                return false;
            }
            let candidates = if inner.config.allow_path_discovery {
                resolve_image_command(&inner.config.prebuilt_candidates, &inner.config.state_dir)
            } else {
                let found = inner
                    .config
                    .prebuilt_candidates
                    .iter()
                    .find(|p| p.is_file())
                    .cloned();
                found.map(ImageCommand::Binary).or_else(|| {
                    std::env::var("HYPER_SCHEME_IMAGE")
                        .ok()
                        .map(PathBuf::from)
                        .filter(|p| p.is_file())
                        .map(ImageCommand::Binary)
                })
            };
            let Some(command) = candidates else {
                if !inner.unavailable_logged {
                    inner.unavailable_logged = true;
                    tracing::info!(
                        target: "scheme_extension",
                        "no scheme image available (no prebuilt binary, no gxi/gsi); \
                         scheme plugins are disabled for this session"
                    );
                }
                inner.respawns = MAX_RESPAWNS; // do not re-probe every dispatch
                return false;
            };
            inner.respawns += 1;
            if inner.respawns > 1 {
                self.metrics.respawns.fetch_add(1, Ordering::Relaxed);
            }
            match ImageHandle::spawn(&command).await {
                Ok(image) => {
                    tracing::info!(
                        target: "scheme_extension",
                        command = %command.describe(),
                        kernel = %image.kernel_version,
                        "scheme image started"
                    );
                    inner.image = Some(image);
                    inner.image_command = Some(command);
                    self.boot_load(inner).await;
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        target: "scheme_extension",
                        command = %command.describe(),
                        error = %e,
                        "failed to start scheme image"
                    );
                    false
                }
            }
        }
    }

    /// Load plugin scripts + replay the journal into a fresh image.
    async fn boot_load(&self, inner: &mut Inner) {
        let sources: Vec<(String, String)> = inner
            .plugins
            .iter()
            .map(|p| (p.meta.name.clone(), p.source.clone()))
            .collect();
        for (name, source) in sources {
            let req = Sexp::list(vec![
                Sexp::sym("load-plugin"),
                Sexp::str(name.clone()),
                Sexp::str(source),
            ]);
            let ok = matches!(
                self.image_request(inner, &req, timeouts::INIT).await,
                CallOutcome::Reply(ref r) if r.head_sym() == Some("ok")
            );
            if ok {
                self.metrics.loads_ok.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.loads_failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "scheme_extension", plugin = %name, "scheme plugin failed to load");
            }
            if let Some(p) = inner.plugins.iter_mut().find(|p| p.meta.name == name) {
                p.load_failed = !ok;
            }
        }
        // Journal replay: per-entry error tolerance (a bad pending entry must
        // never brick boot; commit probes quarantine it).
        match Journal::new(&inner.config.state_dir).load_effective() {
            Ok((redefines, _)) => {
                for r in redefines {
                    let req = Sexp::list(vec![
                        Sexp::sym("redefine"),
                        Sexp::str(r.plugin.clone()),
                        Sexp::sym(&r.event),
                        Sexp::str(r.source),
                    ]);
                    if !matches!(
                        self.image_request(inner, &req, timeouts::INIT).await,
                        CallOutcome::Reply(ref reply) if reply.head_sym() == Some("ok")
                    ) {
                        tracing::warn!(
                            target: "scheme_extension",
                            plugin = %r.plugin,
                            event = %r.event,
                            "journaled redefine failed to apply; skipping"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "scheme_extension", error = %e, "scheme journal unreadable; replay skipped");
            }
        }
    }

    /// Commit probe: boot a second, throwaway image and replay everything.
    async fn run_commit_probe(
        &self,
        inner: &Inner,
        redefines: &[EffectiveRedefine],
    ) -> Result<(), String> {
        let command = if inner.config.allow_path_discovery {
            resolve_image_command(&inner.config.prebuilt_candidates, &inner.config.state_dir)
        } else {
            inner.image_command.clone()
        }
        .ok_or_else(|| "no scheme image available for the commit probe".to_string())?;
        let mut probe = ImageHandle::spawn(&command)
            .await
            .map_err(|e| format!("probe boot failed: {e}"))?;
        let mut fail: Option<String> = None;
        for p in &inner.plugins {
            let req = Sexp::list(vec![
                Sexp::sym("load-plugin"),
                Sexp::str(p.meta.name.clone()),
                Sexp::str(p.source.clone()),
            ]);
            match probe.request(&req, timeouts::INIT).await {
                Ok(r) if r.head_sym() == Some("ok") => {}
                Ok(r) => {
                    fail = Some(format!("plugin `{}` failed replay: {r}", p.meta.name));
                    break;
                }
                Err(e) => {
                    fail = Some(format!("probe io error: {e}"));
                    break;
                }
            }
        }
        if fail.is_none() {
            for r in redefines {
                let req = Sexp::list(vec![
                    Sexp::sym("redefine"),
                    Sexp::str(r.plugin.clone()),
                    Sexp::sym(&r.event),
                    Sexp::str(r.source.clone()),
                ]);
                match probe.request(&req, timeouts::INIT).await {
                    Ok(reply) if reply.head_sym() == Some("ok") => {}
                    Ok(reply) => {
                        fail = Some(format!(
                            "redefine `{}`/`{}` failed replay: {reply}",
                            r.plugin, r.event
                        ));
                        break;
                    }
                    Err(e) => {
                        fail = Some(format!("probe io error: {e}"));
                        break;
                    }
                }
            }
        }
        probe.shutdown().await;
        match fail {
            None => Ok(()),
            Some(detail) => Err(detail),
        }
    }

    /// One dispatch call to one plugin handler.
    async fn call_plugin(
        &self,
        inner: &mut Inner,
        event: &str,
        plugin: &str,
        ctx: Vec<Sexp>,
        timeout: Duration,
    ) -> CallOutcome {
        if !self.ensure_image(inner).await {
            return CallOutcome::Unavailable;
        }
        if inner
            .plugins
            .iter()
            .any(|p| p.meta.name == plugin && p.load_failed)
        {
            return CallOutcome::Failed("plugin failed to load".into());
        }
        let req = Sexp::list(vec![
            Sexp::sym("dispatch"),
            Sexp::sym(event),
            Sexp::str(plugin),
            Sexp::List(ctx),
        ]);
        self.image_request(inner, &req, timeout).await
    }

    /// Raw request against the live image; kills it on transport failure so
    /// the next dispatch respawns (within budget).
    async fn image_request(&self, inner: &mut Inner, req: &Sexp, timeout: Duration) -> CallOutcome {
        let Some(image) = inner.image.as_mut() else {
            return CallOutcome::Unavailable;
        };
        match image.request(req, timeout).await {
            Ok(reply) => CallOutcome::Reply(reply),
            Err(ImageError::Timeout(d)) => {
                tracing::warn!(target: "scheme_extension", "scheme image call timed out; killing image");
                if let Some(mut image) = inner.image.take() {
                    image.kill().await;
                }
                CallOutcome::Timeout(d)
            }
            Err(e) => {
                tracing::warn!(target: "scheme_extension", error = %e, "scheme image call failed; killing image");
                if let Some(mut image) = inner.image.take() {
                    image.kill().await;
                }
                CallOutcome::Failed(e.to_string())
            }
        }
    }

    fn outcome_to_result(&self, plugin: &str, outcome: CallOutcome) -> SchemeCallResult {
        let result = match outcome {
            CallOutcome::Reply(r) => match r.head_sym() {
                Some("no-handler") => SchemeCallResult::NoHandler {
                    plugin: plugin.to_string(),
                },
                Some("err") => SchemeCallResult::HandlerError {
                    plugin: plugin.to_string(),
                    error: r.arg(0).and_then(Sexp::as_str).unwrap_or_default().to_string(),
                },
                _ => SchemeCallResult::Ok {
                    plugin: plugin.to_string(),
                    reply: r.render(),
                },
            },
            CallOutcome::Failed(error) => SchemeCallResult::Failed {
                plugin: plugin.to_string(),
                error,
            },
            CallOutcome::Timeout(limit) => SchemeCallResult::Timeout {
                plugin: plugin.to_string(),
                limit,
            },
            CallOutcome::Unavailable => SchemeCallResult::Unavailable {
                plugin: plugin.to_string(),
            },
        };
        match &result {
            SchemeCallResult::Ok { .. } | SchemeCallResult::NoHandler { .. } => {
                self.metrics.calls_ok.fetch_add(1, Ordering::Relaxed);
            }
            SchemeCallResult::HandlerError { .. } | SchemeCallResult::Failed { .. } => {
                self.metrics.calls_failed.fetch_add(1, Ordering::Relaxed);
            }
            SchemeCallResult::Timeout { .. } => {
                self.metrics.calls_timeout.fetch_add(1, Ordering::Relaxed);
            }
            SchemeCallResult::SkippedCapability { .. } | SchemeCallResult::Unavailable { .. } => {}
        }
        result
    }
}

/// UTF-8-safe byte cap (floors to the previous char boundary).
fn cap_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Pre-tool decision for one outcome. `Some(reason)` = deny.
fn gate_deny_reason(
    outcome: &CallOutcome,
    gate_fail: GateFailMode,
    plugin: &str,
    tool: &str,
) -> Option<String> {
    match outcome {
        CallOutcome::Reply(r) => match r.head_sym() {
            Some("deny") => Some(
                r.arg(0)
                    .and_then(Sexp::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!("denied by scheme extension `{plugin}` (tool `{tool}`)")
                    }),
            ),
            Some("err") if gate_fail == GateFailMode::Closed => Some(format!(
                "scheme extension `{plugin}` failed closed (handler error on tool `{tool}`)"
            )),
            _ => None,
        },
        CallOutcome::Failed(_) | CallOutcome::Timeout(_) if gate_fail == GateFailMode::Closed => {
            Some(format!(
                "scheme extension `{plugin}` failed closed (trap/timeout on tool `{tool}`)"
            ))
        }
        // Whole-feature unavailability is never a gate: fail-open by design.
        _ => None,
    }
}

/// Stop decision for one outcome. `Some(reason)` = block.
fn stop_block_reason(outcome: &CallOutcome, gate_fail: GateFailMode, plugin: &str) -> Option<String> {
    match outcome {
        CallOutcome::Reply(r) => match r.head_sym() {
            Some("block") => Some(
                r.arg(0)
                    .and_then(Sexp::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("blocked by scheme extension `{plugin}`")),
            ),
            Some("err") if gate_fail == GateFailMode::Closed => Some(format!(
                "scheme extension `{plugin}` failed closed (handler error on stop)"
            )),
            _ => None,
        },
        CallOutcome::Failed(_) | CallOutcome::Timeout(_) if gate_fail == GateFailMode::Closed => {
            Some(format!(
                "scheme extension `{plugin}` failed closed (trap/timeout on stop)"
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
