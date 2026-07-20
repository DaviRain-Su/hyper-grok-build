//! Headless single-turn mode (`grok -p "prompt"`).
//!
//! Runs the agent in-process via
//! `spawn_grok_shell`, sends the ACP lifecycle (init → auth → session → prompt),
//! streams text to stdout, and exits cleanly via `CancellationToken`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::ValueEnum;
use tokio_util::sync::CancellationToken;

use agent_client_protocol as acp;
use xai_acp_lib::{AcpAgentTx, AcpClientMessageBox, AcpClientRx, acp_send};
use xai_grok_shell::agent::auth_method::AuthMethodKind;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::extensions::task::{CancelSubagentRequest, KillTaskRequest};
use xai_grok_shell::sampling::error::{
    RATE_LIMITED_ERROR_CODE, error_detail_from_data, format_rate_limited_user_message,
};
use xai_grok_shell::sampling::types::{
    REASONING_EFFORT_META_KEY, parse_canonical_effort_token, reasoning_effort_meta_value,
};
use xai_grok_shell::util::config as cli_config;

use crate::acp::model_state::{EffortTokenError, ModelState};
use crate::acp::spawn::spawn_grok_shell;
use crate::client_identity::{HEADLESS_CLIENT_TYPE, PAGER_CLIENT_VERSION};

// ── Types ────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Plain,
    Json,
    #[value(name = "streaming-json")]
    StreamingJson,
}

pub fn parse_json_schema(input: &str) -> anyhow::Result<serde_json::Value> {
    let schema: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| anyhow::anyhow!("--json-schema: invalid JSON: {e}"))?;
    if !schema.is_object() {
        anyhow::bail!("--json-schema: must be a JSON object describing a JSON Schema");
    }
    Ok(schema)
}

#[derive(Debug, Clone)]
pub enum HeadlessPrompt {
    Text(String),
    Blocks(Vec<acp::ContentBlock>),
}

impl HeadlessPrompt {
    /// Build from mutually-exclusive CLI prompt args. `None` = interactive mode.
    pub fn from_args(
        single: Option<&str>,
        prompt_json: Option<&str>,
        prompt_file: Option<&Path>,
    ) -> anyhow::Result<Option<Self>> {
        if let Some(text) = single {
            Self::from_text(text)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("--single: {e}"))
        } else if let Some(json_str) = prompt_json {
            Self::from_json(json_str)
                .map(Some)
                .map_err(|e| anyhow::anyhow!("--prompt-json: {e}"))
        } else if let Some(path) = prompt_file {
            Self::from_file(path).map(Some)
        } else {
            Ok(None)
        }
    }

    /// `.json` files are parsed as content blocks, everything else as text.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read '{}': {e}", path.display()))?;

        let context = |e| anyhow::anyhow!("'{}': {e}", path.display());
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            Self::from_json(&content).map_err(context)
        } else {
            Self::from_text(&content).map_err(context)
        }
    }

    fn from_text(text: &str) -> anyhow::Result<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            anyhow::bail!("prompt is empty");
        }
        Ok(Self::Text(trimmed.to_string()))
    }

    fn from_json(json_str: &str) -> anyhow::Result<Self> {
        let blocks = parse_prompt_json(json_str)?;
        Ok(Self::Blocks(blocks))
    }

    pub fn into_content_blocks(self) -> Vec<acp::ContentBlock> {
        match self {
            Self::Text(text) => vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
            Self::Blocks(blocks) => blocks,
        }
    }
}

/// Parse a JSON string into ACP content blocks.
///
/// Accepts an array (`[...]`) or typed wrapper (`{"type":"acp","content":[...]}`).
fn parse_prompt_json(json_str: &str) -> anyhow::Result<Vec<acp::ContentBlock>> {
    let value: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("Invalid JSON: {e}"))?;

    let blocks: Vec<acp::ContentBlock> = match value {
        serde_json::Value::Array(_) => serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("Invalid ACP content blocks: {e}"))?,

        serde_json::Value::Object(ref map) => {
            let format_type = map.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "JSON object must have a \"type\" field \
                         (e.g., {{\"type\": \"acp\", \"content\": [...]}})"
                )
            })?;
            let content = map
                .get("content")
                .ok_or_else(|| anyhow::anyhow!("JSON object must have a \"content\" field"))?;

            match format_type {
                "acp" => serde_json::from_value(content.clone()).map_err(|e| {
                    anyhow::anyhow!("Invalid ACP content blocks in \"content\": {e}")
                })?,
                other => anyhow::bail!(
                    "Unsupported prompt format type: \"{other}\". Supported types: \"acp\""
                ),
            }
        }

        _ => {
            anyhow::bail!("Expected JSON array or {{\"type\": \"...\", \"content\": [...]}} object")
        }
    };

    if blocks.is_empty() {
        anyhow::bail!("content blocks array is empty");
    }
    Ok(blocks)
}

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub session_id: Option<String>,
    pub resume: Option<String>,
    pub cwd: Option<PathBuf>,
    pub yolo: bool,
    pub trust: bool,
    pub output_format: OutputFormat,
    pub json_schema: Option<serde_json::Value>,
    pub model: Option<String>,
    pub rules: Option<String>,
    pub system_prompt_override: Option<String>,
    pub continue_last_session: bool,
    /// Fork on resume/continue (`--fork-session`).
    pub fork_session: bool,
    pub worktree: Option<String>,
    pub restore_code: bool,
    pub agent: Option<String>,
    pub agents_json: Option<String>,
    pub cli_tools: Option<String>,
    pub cli_disallowed_tools: Option<String>,
    pub disable_web_search: bool,
    pub allow_rules: Vec<String>,
    pub deny_rules: Vec<String>,
    pub max_turns: Option<u32>,
    pub permission_mode_flag: Option<String>,
    /// Effort token (`--reasoning-effort` / `--effort`); resolved like `/effort` after models load.
    pub reasoning_effort: Option<String>,
    /// Append a self-verification loop after the prompt completes.
    pub self_verify: bool,
    /// Run the task N ways in parallel and pick the best.
    pub best_of_n: Option<u32>,
    /// Wait for background tasks (bash, subagent, monitor) to report
    /// `task_completed` before exiting. Default: true. Does not wait for
    /// server-side auto-wake (that runs inside the shell). Use
    /// `--no-wait-for-background` for fast smoke tests; long-lived monitors
    /// are capped by `background_wait_timeout`.
    pub wait_for_background: bool,
    /// Max time to wait for background quiescence after the first turn ends.
    pub background_wait_timeout: Duration,
}

// ── CLI flag helpers ─────────────────────────────────────────────────────

/// Parse a comma-separated list into a vec, or None if empty.
fn parse_comma_list(s: Option<&str>) -> Option<Vec<String>> {
    s.and_then(|s| {
        let v: Vec<String> = s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        if v.is_empty() { None } else { Some(v) }
    })
}

pub fn parse_permission_rules_strict(
    allow: &[String],
    deny: &[String],
) -> anyhow::Result<Vec<xai_grok_workspace::permission::types::PermissionRule>> {
    let (rules, errors) = parse_permission_rules_inner(allow, deny);
    if !errors.is_empty() {
        let msgs: Vec<String> = errors
            .into_iter()
            .map(|(flag, rule, err)| format!("{flag} \"{rule}\": {err}"))
            .collect();
        anyhow::bail!("{}", msgs.join("; "));
    }
    Ok(rules)
}

pub fn parse_permission_rules_lenient(
    allow: &[String],
    deny: &[String],
) -> Vec<xai_grok_workspace::permission::types::PermissionRule> {
    let (rules, errors) = parse_permission_rules_inner(allow, deny);
    for (flag, rule, err) in errors {
        eprintln!("warning: {flag} \"{rule}\": {err}, skipping");
    }
    rules
}

// Deny rules are processed before allow rules so that after prepending
// to the config's rule list the order is [cli_deny, cli_allow, config_rules...].
// The policy evaluator is order-independent (deny > ask > allow), so this
// ordering is cosmetic for logging/provenance, not functional.
pub(crate) fn parse_permission_rules_inner(
    allow: &[String],
    deny: &[String],
) -> (
    Vec<xai_grok_workspace::permission::types::PermissionRule>,
    Vec<(&'static str, String, String)>,
) {
    use xai_grok_workspace::permission::rules::parse_permission_rule;
    use xai_grok_workspace::permission::types::RuleAction;

    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for rule_str in deny {
        match parse_permission_rule(rule_str, RuleAction::Deny) {
            Ok(rule) => rules.push(rule),
            Err(e) => errors.push(("--deny", rule_str.clone(), e.to_string())),
        }
    }
    for rule_str in allow {
        match parse_permission_rule(rule_str, RuleAction::Allow) {
            Ok(rule) => rules.push(rule),
            Err(e) => errors.push(("--allow", rule_str.clone(), e.to_string())),
        }
    }
    (rules, errors)
}

pub(crate) enum ResolvedAgent {
    FilePath(PathBuf),
    Name(String),
}

pub(crate) fn resolve_agent_arg(agent: &str) -> ResolvedAgent {
    let path = std::path::Path::new(agent);
    if path.exists() && path.is_file() {
        ResolvedAgent::FilePath(dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    } else {
        ResolvedAgent::Name(agent.to_string())
    }
}

fn parse_cli_agents(
    json: &str,
) -> anyhow::Result<Vec<xai_grok_shell::agent::config::AgentDefinition>> {
    let map: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("--agents: invalid JSON: {e}"))?;
    let mut agents = Vec::with_capacity(map.len());
    for (name, mut value) in map {
        if let serde_json::Value::Object(ref mut obj) = value {
            // Accept "prompt" as an alias for "promptBody".
            if !obj.contains_key("promptBody")
                && let Some(prompt) = obj.remove("prompt")
            {
                obj.insert("promptBody".to_string(), prompt);
            }
            obj.entry("name".to_string())
                .or_insert_with(|| serde_json::Value::String(name.clone()));
            obj.entry("description".to_string())
                .or_insert_with(|| serde_json::Value::String(name.clone()));
        }
        let mut def = xai_grok_shell::agent::config::AgentDefinition::from_json(&value)
            .map_err(|e| anyhow::anyhow!("--agents: failed to parse '{name}': {e}"))?;
        def.name = name;
        agents.push(def);
    }
    Ok(agents)
}

fn apply_agent_flag(agent: &Option<String>, config: &mut xai_grok_shell::agent::config::Config) {
    if let Some(agent) = agent {
        match resolve_agent_arg(agent) {
            ResolvedAgent::FilePath(path) => config.agent_profile_path = Some(path),
            ResolvedAgent::Name(name) => config.agent.name = Some(name),
        }
    }
}

// ── Emitter ──────────────────────────────────────────────────────────────

struct HeadlessEmitter {
    format: OutputFormat,
    parse_structured_output: bool,
    text_buffer: String,
    thought_buffer: String,
    /// Agent's schema-validated output (both backends), read from the
    /// prompt-response `_meta`.
    structured_output: Option<Result<serde_json::Value, String>>,
    /// From `_meta.usage`, projected onto the final result when present.
    usage: Option<serde_json::Value>,
}

impl HeadlessEmitter {
    fn new(format: OutputFormat, parse_structured_output: bool) -> Self {
        Self {
            format,
            parse_structured_output,
            text_buffer: String::new(),
            thought_buffer: String::new(),
            structured_output: None,
            usage: None,
        }
    }

    /// Read structured output from the prompt-response `_meta` — the same
    /// object headless awaits for `sessionId`/`requestId`, so delivery is
    /// deterministic (no side-channel race). `structuredOutput` carries the
    /// value, `structuredOutputError` the failure; absence leaves `None`.
    fn set_structured_output_from_meta(&mut self, meta: Option<&acp::Meta>) {
        if !self.parse_structured_output {
            return;
        }
        let Some(meta) = meta else { return };
        if let Some(err) = meta.get("structuredOutputError").and_then(|v| v.as_str()) {
            self.structured_output = Some(Err(err.to_string()));
        } else if let Some(value) = meta.get("structuredOutput") {
            self.structured_output = Some(Ok(value.clone()));
        }
    }

    fn set_usage_from_meta(&mut self, meta: Option<&acp::Meta>) {
        let Some(meta) = meta else { return };
        self.usage = meta.get("usage").cloned();
    }

    fn on_text_chunk(&mut self, text: &str) {
        match self.format {
            OutputFormat::Plain => {
                use std::io::Write as _;
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            OutputFormat::StreamingJson => {
                println!("{}", serde_json::json!({"type":"text","data": text}));
                if self.parse_structured_output {
                    self.text_buffer.push_str(text);
                }
            }
            OutputFormat::Json => {
                self.text_buffer.push_str(text);
            }
        }
    }

    fn on_thought_chunk(&mut self, text: &str) {
        match self.format {
            OutputFormat::Plain => { /* no-op */ }
            OutputFormat::StreamingJson => {
                println!("{}", serde_json::json!({"type":"thought","data": text}));
            }
            OutputFormat::Json => {
                self.thought_buffer.push_str(text);
            }
        }
    }

    fn attach_structured_output(&self, target: &mut serde_json::Value) {
        if !self.parse_structured_output {
            return;
        }
        // The agent is the only source of validated output; never parse the raw
        // text buffer (that would bypass validation). Absent `_meta` output
        // (max-turns/cancel) → a clean error, never unvalidated JSON.
        let result = self
            .structured_output
            .clone()
            .unwrap_or_else(|| Err("model did not produce structured output".to_string()));
        match result {
            Ok(value) => {
                target["structuredOutput"] = value;
            }
            Err(e) => {
                target["structuredOutput"] = serde_json::Value::Null;
                target["structuredOutputError"] = e.into();
            }
        }
    }

    /// Final object for `--output-format json`, including spend fields when present.
    fn build_json_result(
        &self,
        stop_reason: &str,
        session_id: &str,
        request_id: &str,
    ) -> serde_json::Value {
        let mut result = serde_json::json!({
            "text": self.text_buffer,
            "stopReason": stop_reason,
            "sessionId": session_id,
            "requestId": request_id
        });
        if !self.thought_buffer.is_empty() {
            result["thought"] = serde_json::Value::String(self.thought_buffer.clone());
        }
        if let Some(usage) = &self.usage {
            attach_result_usage(&mut result, usage);
        }
        self.attach_structured_output(&mut result);
        result
    }

    fn on_end(&mut self, stop_reason: &str, session_id: &str, request_id: &str) {
        match self.format {
            OutputFormat::Plain => {
                println!();
            }
            OutputFormat::StreamingJson => {
                let mut end = serde_json::json!({
                    "type": "end",
                    "stopReason": stop_reason,
                    "sessionId": session_id,
                    "requestId": request_id
                });
                if let Some(usage) = &self.usage {
                    attach_result_usage(&mut end, usage);
                }
                self.attach_structured_output(&mut end);
                println!("{end}");
            }
            OutputFormat::Json => {
                let result = self.build_json_result(stop_reason, session_id, request_id);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
                );
            }
        }
    }

    fn on_error(&self, message: &str) {
        match self.format {
            OutputFormat::Plain => eprintln!("{message}"),
            OutputFormat::StreamingJson | OutputFormat::Json => {
                let mut err = serde_json::json!({"type":"error","message": message});
                if let Some(usage) = &self.usage {
                    attach_result_usage(&mut err, usage);
                }
                println!("{err}");
            }
        }
    }
}

fn attach_result_usage(result: &mut serde_json::Value, usage: &serde_json::Value) {
    xai_grok_shell::extensions::notification::attach_result_usage_fail_closed(result, usage);
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn auto_respond_to_permissions(
    args: &acp::RequestPermissionRequest,
    option_kinds: &[acp::PermissionOptionKind],
) -> Option<acp::RequestPermissionResponse> {
    for &option_kind in option_kinds {
        for option in &args.options {
            if option.kind == option_kind {
                return Some(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        option.option_id.clone(),
                    )),
                ));
            }
        }
    }
    None
}

/// "Not signed in" error message, tailored to the session type.
fn auth_required_message(interactive: bool) -> String {
    if interactive {
        "Not signed in. Run `grok login` to authenticate \
         (or `grok login --device-code` if no browser is available)."
            .to_string()
    } else {
        "Not signed in. To authenticate without a browser, run:\n  \
         grok login --device-code\n\n\
         Alternatively, set the XAI_API_KEY environment variable \
         or run `grok login` on a machine with a browser."
            .to_string()
    }
}

/// Authenticate using the agent's `defaultAuthMethodId` (source of truth for
/// `[auth] preferred_method`). Fail closed when no method is available — do not
/// invent api_key vs session ordering client-side.
///
/// Returns whether the selected method is API-key auth (for rate-limit copy).
async fn authenticate(
    acp_tx: &AcpAgentTx,
    auths: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
) -> anyhow::Result<bool> {
    let method_id = crate::acp::select_eager_auth_method(auths, default_auth_method_id)
        .ok_or_else(|| {
            use std::io::IsTerminal;
            let interactive = std::io::stdin().is_terminal()
                && !xai_grok_shell::util::clipboard::is_remote_session();
            anyhow::anyhow!("{}", auth_required_message(interactive))
        })?;
    let kind = AuthMethodKind::from_id(&method_id);
    // Prefer non-interactive methods only; interactive login is not usable headless.
    if kind.needs_interactive_login() {
        use std::io::IsTerminal;
        let interactive =
            std::io::stdin().is_terminal() && !xai_grok_shell::util::clipboard::is_remote_session();
        anyhow::bail!("{}", auth_required_message(interactive));
    }
    let is_api_key_auth = kind.is_api_key();
    let _resp: acp::AuthenticateResponse = acp_send(
        acp::AuthenticateRequest::new(method_id)
            .meta(serde_json::json!({"headless": true}).as_object().cloned()),
        acp_tx,
    )
    .await?;
    Ok(is_api_key_auth)
}

fn build_headless_init_request(
    rules: Option<&str>,
    system_prompt_override: Option<&str>,
) -> acp::InitializeRequest {
    let mut meta = serde_json::json!({
        "clientType": HEADLESS_CLIENT_TYPE,
        "clientVersion": PAGER_CLIENT_VERSION,
    });
    if let Some(rules) = rules {
        meta["rules"] = serde_json::json!(rules);
    }
    if let Some(system_prompt_override) = system_prompt_override {
        meta["systemPromptOverride"] = serde_json::json!(system_prompt_override);
    }
    meta["startupHints"] = serde_json::json!({
        "nonInteractive": true,
        "skipGitStatus": true,
        "skipProjectLayout": true,
    });

    acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        )
        .meta(meta.as_object().cloned())
}

/// Extract the body of a compiled-in SKILL.md (strip YAML frontmatter).
fn skill_body(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return trimmed;
    }
    if let Some(rest) = trimmed.get(3..)
        && let Some(end) = rest.find("\n---")
    {
        return rest[end + 4..].trim_start();
    }
    trimmed
}

struct OpenedSession {
    session_id: acp::SessionId,
    models: ModelState,
}

async fn open_session(
    acp_tx: &AcpAgentTx,
    cwd: &Path,
    session_id_flag: Option<&str>,
    restore_code: Option<bool>,
) -> anyhow::Result<OpenedSession> {
    // Pager opens sessions before the agent resolves per-vendor compat;
    // default (all-on) preserves existing behavior — the agent applies
    // the resolved config once the session is live.
    let mcp_servers =
        cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default());

    if let Some(sid) = session_id_flag {
        let try_load: Result<acp::LoadSessionResponse, _> = acp_send(
            acp::LoadSessionRequest::new(acp::SessionId::new(sid.to_string()), cwd.to_path_buf())
                .mcp_servers(mcp_servers.clone())
                .meta({
                    let mut m = acp::Meta::new();
                    m.insert("noReplay".into(), serde_json::Value::Bool(true));
                    if let Some(true) = restore_code {
                        m.insert("x.ai/restore_code".into(), serde_json::Value::Bool(true));
                    }
                    Some(m)
                }),
            acp_tx,
        )
        .await;
        if let Ok(resp) = try_load {
            return Ok(OpenedSession {
                session_id: acp::SessionId::new(sid.to_string()),
                models: ModelState::from(resp.models),
            });
        }
        anyhow::bail!("Session does not exist");
    }

    let new_resp: acp::NewSessionResponse = acp_send(
        acp::NewSessionRequest::new(cwd.to_path_buf()).mcp_servers(mcp_servers),
        acp_tx,
    )
    .await?;
    Ok(OpenedSession {
        session_id: new_resp.session_id,
        models: ModelState::from(new_resp.models),
    })
}

async fn open_session_with_id(
    acp_tx: &AcpAgentTx,
    cwd: &Path,
    session_id: &str,
) -> anyhow::Result<OpenedSession> {
    let cwd_str = cwd.to_string_lossy();
    crate::app::session_startup::ensure_session_id_available(session_id, &cwd_str)?;
    let mcp_servers =
        cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default());
    let new_resp: acp::NewSessionResponse = acp_send(
        acp::NewSessionRequest::new(cwd.to_path_buf())
            .mcp_servers(mcp_servers)
            .meta(
                serde_json::json!({ "sessionId": session_id })
                    .as_object()
                    .cloned(),
            ),
        acp_tx,
    )
    .await?;
    Ok(OpenedSession {
        session_id: new_resp.session_id,
        models: ModelState::from(new_resp.models),
    })
}

async fn fork_then_open(
    acp_tx: &AcpAgentTx,
    launch_cwd: &Path,
    parent_id: &str,
    parent_cwd: Option<&Path>,
    new_id: Option<&str>,
    restore_code: Option<bool>,
) -> anyhow::Result<OpenedSession> {
    use crate::app::session_startup::{
        effective_fork_new_cwd, ensure_session_id_available, fork_response_error,
        fork_response_new_session_id, fork_session_params, parent_session_is_worktree,
    };
    let launch_cwd_str = launch_cwd.to_string_lossy().into_owned();
    // Align with interactive: child lands under parent session cwd when the
    // parent was resolved from another directory (`newCwd` = parent_cwd).
    let new_cwd_str = effective_fork_new_cwd(&launch_cwd_str, parent_cwd);
    let write_cwd = PathBuf::from(&new_cwd_str);
    if let Some(nid) = new_id {
        ensure_session_id_available(nid, &new_cwd_str)?;
    }
    let parent_is_worktree = parent_session_is_worktree(parent_id, &write_cwd);
    let payload = fork_session_params(parent_id, &write_cwd, new_id, parent_is_worktree);
    let req = acp::ExtRequest::new(
        "x.ai/session/fork",
        serde_json::value::to_raw_value(&payload)
            .expect("serialize fork params")
            .into(),
    );
    let resp = acp_send(req, acp_tx).await?;
    if let Some(err) = fork_response_error(resp.0.get()) {
        anyhow::bail!("fork failed: {err}");
    }
    let child = fork_response_new_session_id(resp.0.get())
        .ok_or_else(|| anyhow::anyhow!("fork response missing newSessionId"))?;
    match open_session(acp_tx, &write_cwd, Some(&child), restore_code).await {
        Ok(opened) => Ok(opened),
        Err(e) => Err(anyhow::anyhow!(
            "fork succeeded as {child} but load failed: {e}"
        )),
    }
}

/// Apply `-m` / effort after session open (via `resolve_effort_for_model`, then
/// SetSessionModel).
///
/// Headless maps the classified [`EffortTokenError`] differently from the TUI: a
/// one-shot run soft-ignores effort on a non-supporting model (still applying
/// `-m`) but hard-fails on a genuinely unknown token. The TUI instead keeps the
/// `-m` switch and only toasts — intentional, since headless has no scrollback
/// to carry a non-fatal warning.
async fn apply_headless_model_and_effort(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    models: &ModelState,
    model_name: Option<&str>,
    effort_token: Option<&str>,
) -> anyhow::Result<()> {
    if model_name.is_none() && effort_token.is_none() {
        return Ok(());
    }

    let model_id = if let Some(name) = model_name {
        models
            .resolve_by_name_or_id(name)
            .unwrap_or_else(|| acp::ModelId::new(name))
    } else {
        models.current.clone().ok_or_else(|| {
            anyhow::anyhow!("--effort/--reasoning-effort: no active model to apply effort to")
        })?
    };

    let effort = match effort_token {
        None => None,
        // Pre-catalog: the canonical token was already stamped into the agent
        // config; a remapped menu id can't resolve without a loaded catalog.
        Some(token) if models.available.is_empty() => {
            if parse_canonical_effort_token(token).is_none() {
                // Do not hardcode a level list here: without a catalog we cannot
                // know what the model offers, and advertising none/minimal/… has
                // led users to try values that then 400 on the API.
                anyhow::bail!(
                    "--effort/--reasoning-effort: unknown effort level '{token}' \
                     (model catalog unavailable; remapped menu ids require a loaded catalog)"
                );
            }
            None
        }
        Some(token) => match models.resolve_effort_for_model(&model_id, token) {
            Ok(effort) => Some(effort),
            // Soft-ignore effort on a non-supporting model; still apply `-m`.
            Err(EffortTokenError::Unsupported) => {
                tracing::warn!(
                    model = %model_id.0,
                    token,
                    "--effort/--reasoning-effort: model does not support reasoning effort; ignoring"
                );
                None
            }
            Err(err) => anyhow::bail!("--effort/--reasoning-effort: {}", err.message()),
        },
    };

    // Nothing to apply (effort pre-stamped or ignored, and no model override):
    // skip the no-op SetSessionModel.
    if model_name.is_none() && effort.is_none() {
        return Ok(());
    }

    let meta = effort.map(|eff| {
        let mut m = acp::Meta::new();
        m.insert(
            REASONING_EFFORT_META_KEY.to_string(),
            reasoning_effort_meta_value(eff),
        );
        m
    });

    acp_send(
        acp::SetSessionModelRequest::new(session_id.clone(), model_id.clone()).meta(meta),
        acp_tx,
    )
    .await
    .map_err(|e| {
        if let Some(name) = model_name {
            anyhow::anyhow!(
                "Couldn't set model '{}': {}. Run 'grok models' to see available models.",
                name,
                e
            )
        } else {
            anyhow::anyhow!("Couldn't apply reasoning effort: {e}")
        }
    })?;
    tracing::debug!(
        model_id = %model_id.0,
        effort = ?effort,
        "headless: model/effort set"
    );
    Ok(())
}

// ── Main entry point ─────────────────────────────────────────────────────

/// Startup-materialization context for headless (`-p`) runs. Never chat:
/// `HeadlessOptions` carries no chat flag, so headless resume targets are
/// always disk/GCS Build sessions.
fn headless_materialize_ctx(has_worktree: bool) -> crate::app::session_startup::MaterializeCtx {
    crate::app::session_startup::MaterializeCtx {
        has_worktree,
        allow_remote_restore: false,
        chat_mode: false,
    }
}

/// Run a headless single-turn prompt.
///
/// Spawns the agent in-process, runs the full ACP lifecycle (init → auth →
/// session → prompt), streams output to stdout, and returns cleanly.
pub async fn run_single_turn(
    prompt: HeadlessPrompt,
    verbatim: bool,
    options: HeadlessOptions,
) -> Result<()> {
    // Stamp proxy requests as headless before the agent spawns and issues
    // its first request (auth enrichment, model list, etc.).
    xai_grok_shell::http::set_process_client_mode_headless();

    let cwd = match options.cwd {
        None => std::env::current_dir()?,
        Some(ref p) => dunce::canonicalize(p)?,
    };

    let mut emitter = HeadlessEmitter::new(options.output_format, options.json_schema.is_some());

    // Load config and spawn agent
    let t_spawn = Instant::now();
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    let mut agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;

    // Canonical-only early stamp; remaps need the post-session catalog resolve below.
    if let Some(ref token) = options.reasoning_effort
        && let Some(effort) = parse_canonical_effort_token(token)
    {
        agent_config.reasoning_effort_override = Some(effort);
    }
    // So initial system prompt / `system_prompt_label` use `-m`, not a later SetSessionModel.
    if let Some(ref model) = options.model {
        agent_config.default_model_override = Some(model.clone());
    }

    agent_config.resolve_runtime_fields(&xai_grok_shell::agent::config::RuntimeResolutionContext {
        raw_config: &raw_config,
        remote_settings: None,
        is_headless: true,
        cli_subagents: None,
        cli_web_search_model: None,
        cli_session_summary_model: None,
        cli_experimental_memory: false,
        cli_no_memory: false,
        disable_web_search: options.disable_web_search,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });

    agent_config.mode = xai_grok_shell::agent::config::AgentMode::Headless;
    agent_config.default_yolo_mode = options.yolo;
    // Remote arg is None: the remote settings permission_mode soft-default is
    // TUI-only; headless runs must not change permission behavior on a
    // remote flag flip.
    agent_config.default_auto_mode = xai_grok_shell::util::config::effective_auto_for_launch(
        options.yolo,
        options.permission_mode_flag.as_deref(),
        None,
    );

    // No agent-level hub client URL (gateway-only cloud; workspace provider
    // hub_url lives on `grok workspace` / WorkspaceStartArgs only).

    apply_agent_flag(&options.agent, &mut agent_config);

    if let Some(ref json) = options.agents_json {
        agent_config.cli_agents = parse_cli_agents(json)?;
    }

    agent_config.cli_agent_overrides = xai_grok_shell::agent::config::CliAgentOverrides {
        tools: parse_comma_list(options.cli_tools.as_deref()),
        disallowed_tools: parse_comma_list(options.cli_disallowed_tools.as_deref()),
        permission_rules: parse_permission_rules_strict(&options.allow_rules, &options.deny_rules)?,
        max_turns: options.max_turns,
        permission_mode: options
            .permission_mode_flag
            .as_deref()
            .map(|s| {
                serde_json::from_value(serde_json::Value::String(s.to_string()))
                    .map_err(|e| anyhow::anyhow!("--permission-mode: invalid value: {e}"))
            })
            .transpose()?,
    };

    // Persist an explicit --trust grant before the agent starts.
    if options.trust {
        xai_grok_shell::agent::folder_trust::grant_folder_trust(&cwd);
    }

    let cancel = CancellationToken::new();
    let memory_config = agent_config.memory_config.clone();
    let spawned = match spawn_grok_shell(agent_config, &cancel, memory_config).await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("Couldn't start session: {e}");
            emitter.on_error(&msg);
            anyhow::bail!("{msg}");
        }
    };
    let (acp_tx, mut acp_rx) = (spawned.channel.tx, spawned.channel.rx);
    crate::unified_log::init(acp_tx.clone());
    crate::unified_log::info(
        "pager started",
        None,
        Some(serde_json::json!({"mode": "headless"})),
    );
    crate::unified_log::flush();

    // Initialize with headless hints
    let init_req = build_headless_init_request(
        options.rules.as_deref(),
        options.system_prompt_override.as_deref(),
    );
    let init_resp: acp::InitializeResponse = match acp_send(init_req, &acp_tx).await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Couldn't initialize: {e}");
            emitter.on_error(&msg);
            cancel.cancel();
            anyhow::bail!("{msg}");
        }
    };
    tracing::debug!(
        elapsed_ms = t_spawn.elapsed().as_millis() as u64,
        "headless: spawn + initialize complete"
    );

    // Authenticate using agent defaultAuthMethodId (preferred_method pin).
    let t_auth = Instant::now();
    let default_auth_method_id = crate::acp::parse_default_auth_method_id(init_resp.meta.as_ref());
    let is_api_key_auth = match authenticate(
        &acp_tx,
        &init_resp.auth_methods,
        default_auth_method_id.as_ref(),
    )
    .await
    {
        Ok(is_api_key) => is_api_key,
        Err(e) => {
            emitter.on_error(&e.to_string());
            cancel.cancel();
            return Err(e);
        }
    };
    tracing::debug!(
        elapsed_ms = t_auth.elapsed().as_millis() as u64,
        "headless: authenticate complete"
    );

    // Same intent + materialize path as interactive (shared SSOT).
    use crate::app::session_startup::{self, MaterializedStartup, SessionStartupFlags};
    let has_resume_id = options.resume.as_deref().filter(|s| !s.is_empty());
    let resume_most_recent = options.resume.as_deref() == Some("");
    let intent = session_startup::session_startup_intent_from_flags(SessionStartupFlags {
        session_id: options.session_id.as_deref(),
        resume_session_id: has_resume_id,
        resume_most_recent,
        continue_last_session: options.continue_last_session,
        fork_session: options.fork_session,
        has_worktree: options.worktree.is_some(),
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let cwd_str = cwd.to_string_lossy().to_string();
    let materialized = session_startup::materialize_startup_for_cwd(
        headless_materialize_ctx(options.worktree.is_some()),
        intent,
        &cwd_str,
    )
    .await?;

    // Open session
    let restore_code = options.restore_code.then_some(true);
    let t_session = Instant::now();
    let opened = match materialized {
        MaterializedStartup::NewAuto => open_session(&acp_tx, &cwd, None, None).await,
        MaterializedStartup::NewWithId { session_id } => {
            open_session_with_id(&acp_tx, &cwd, &session_id).await
        }
        MaterializedStartup::Resume {
            session_id,
            original_cwd,
            ..
        } => {
            let load_cwd = original_cwd.as_deref().unwrap_or(cwd.as_path());
            open_session(&acp_tx, load_cwd, Some(session_id.as_str()), restore_code).await
        }
        MaterializedStartup::Fork {
            parent_session_id,
            parent_cwd,
            new_session_id,
            ..
        } => {
            fork_then_open(
                &acp_tx,
                &cwd,
                &parent_session_id,
                parent_cwd.as_deref(),
                new_session_id.as_deref(),
                restore_code,
            )
            .await
        }
    };
    let OpenedSession {
        session_id,
        models: session_models,
    } = match opened {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Couldn't create session: {e}");
            emitter.on_error(&msg);
            cancel.cancel();
            anyhow::bail!("{msg}");
        }
    };
    tracing::debug!(
        elapsed_ms = t_session.elapsed().as_millis() as u64,
        session_id = %session_id.0,
        "headless: open_session complete"
    );

    // Debug: track headless sessions in active_sessions.json when env var is set.
    let track_active = std::env::var("GROK_TRACK_HEADLESS").is_ok();
    if track_active {
        let _ = xai_grok_shell::active_sessions::register(
            xai_grok_shell::active_sessions::ActiveSession {
                session_id: session_id.clone(),
                pid: std::process::id(),
                cwd: cwd.display().to_string(),
                opened_at: chrono::Utc::now(),
            },
        );
    }

    if let Err(e) = apply_headless_model_and_effort(
        &acp_tx,
        &session_id,
        &session_models,
        options.model.as_deref(),
        options.reasoning_effort.as_deref(),
    )
    .await
    {
        let msg = e.to_string();
        emitter.on_error(&msg);
        cancel.cancel();
        anyhow::bail!("{msg}");
    }

    // Send prompt and stream response
    let mut prompt_blocks = prompt.into_content_blocks();

    // --check / --self-verify: append the check skill AFTER the user prompt
    // so the model completes the task first, then runs verification.
    if options.self_verify {
        prompt_blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
            skill_body(xai_grok_shell::builtin::CHECK_SKILL_MD).to_string(),
        )));
    }

    // --best-of-n N: prefix the user prompt with the compiled-in best-of-n
    // skill content and the candidate count.
    if let Some(n) = options.best_of_n {
        let n = n.clamp(2, 10);
        {
            prompt_blocks.insert(
                0,
                acp::ContentBlock::Text(acp::TextContent::new(format!(
                    "{}\n\n## Number of candidates: {n}",
                    skill_body(xai_grok_shell::builtin::BEST_OF_N_SKILL_MD)
                ))),
            );
        }
    }

    let prompt_meta = {
        let mut meta = serde_json::Map::new();
        if verbatim {
            meta.insert("verbatim".to_string(), serde_json::Value::Bool(true));
        }
        if let Some(ref schema) = options.json_schema {
            meta.insert("outputSchema".to_string(), schema.clone());
        }
        // Screen-mode telemetry (`prompt_submitted.screen_mode`): headless is
        // its own mode, distinct from the TUI's fullscreen/inline/minimal.
        meta.insert(
            "screenMode".to_string(),
            serde_json::Value::String("headless".to_string()),
        );
        Some(meta)
    };

    let request = acp::PromptRequest::new(session_id.clone(), prompt_blocks).meta(prompt_meta);
    let t_prompt = Instant::now();
    let mut ttf_logged = false;
    let mut prompt_fut = Box::pin(acp_send(request, &acp_tx));
    let mut prompt_result = None;
    // Pending background work: bash/monitor via x.ai/task_backgrounded +
    // task_completed; background subagents via SubagentSpawned + SubagentFinished
    // on x.ai/session_notification (prefixed `subagent:{id}` in pending_bg).
    // Tracked regardless of wait_for_background so the exit reaper always
    // sees still-running work; the flag only gates waiting.
    // No idle/quiet polling and no wait for server-side auto-wake text — exit
    // when lifecycle sets are empty. Auto-wake may still be in flight at exit.
    let mut pending_bg: HashSet<String> = HashSet::new();
    // task_completed can arrive before task_backgrounded; remember those IDs
    // so a late backgrounded does not re-arm waiting.
    let mut completed_before_bg: HashSet<String> = HashSet::new();
    let mut prompt_done_at: Option<Instant> = None;

    loop {
        // First turn done and no tracked bg/monitor tasks still running.
        // Drain buffered ACP first: PromptResponse can complete while
        // task_backgrounded is still queued on acp_rx (never reached select!).
        if options.wait_for_background && prompt_result.is_some() && pending_bg.is_empty() {
            while let Ok(msg) = acp_rx.try_recv() {
                handle_headless_acp_message(
                    msg.boxed(),
                    &mut emitter,
                    t_prompt,
                    &mut ttf_logged,
                    options.yolo,
                    options.output_format,
                    &mut pending_bg,
                    &mut completed_before_bg,
                );
            }
            if pending_bg.is_empty() {
                tracing::debug!("headless: no pending background tasks, exiting");
                break;
            }
        }

        // Safety valve so evals don't hang on long-lived monitors or stuck tasks.
        if options.wait_for_background
            && let Some(done_at) = prompt_done_at
            && done_at.elapsed() >= options.background_wait_timeout
        {
            tracing::warn!(
                pending_bg = pending_bg.len(),
                timeout_secs = options.background_wait_timeout.as_secs(),
                "headless: background wait timed out, exiting"
            );
            break;
        }

        // Only needed while waiting on tasks (timeout enforcement); otherwise
        // the loop blocks on ACP until task_completed or PromptResponse.
        let timeout_deadline = if options.wait_for_background
            && prompt_result.is_some()
            && !pending_bg.is_empty()
            && let Some(done_at) = prompt_done_at
        {
            let remaining = options
                .background_wait_timeout
                .saturating_sub(done_at.elapsed());
            if remaining.is_zero() {
                Duration::from_millis(50)
            } else {
                remaining
            }
        } else {
            Duration::from_secs(3600)
        };

        tokio::select! {
            biased;
            msg = acp_rx.recv() => {
                let Some(msg) = msg else {
                    emitter.on_error("Connection closed unexpectedly");
                    cancel.cancel();
                    anyhow::bail!("Connection closed unexpectedly");
                };
                handle_headless_acp_message(
                    msg.boxed(),
                    &mut emitter,
                    t_prompt,
                    &mut ttf_logged,
                    options.yolo,
                    options.output_format,
                    &mut pending_bg,
                    &mut completed_before_bg,
                );
            }
            res = &mut prompt_fut, if prompt_result.is_none() => {
                prompt_result = Some(res);
                prompt_done_at = Some(Instant::now());
                if !options.wait_for_background {
                    drain_acp_with_grace(
                        &mut acp_rx,
                        Duration::from_millis(750),
                        &mut emitter,
                        t_prompt,
                        &mut ttf_logged,
                        options.yolo,
                        options.output_format,
                        &mut pending_bg,
                        &mut completed_before_bg,
                    )
                    .await;
                    break;
                }
                // With wait_for_background: keep draining ACP for task_completed.
            }
            _ = tokio::time::sleep(timeout_deadline), if options.wait_for_background
                && prompt_result.is_some()
                && !pending_bg.is_empty() =>
            {
                // Wake to re-check background_wait_timeout at the top of the loop.
            }
        }
    }

    // Track lifecycle notifications still queued at loop exit so the reaper
    // sees them (the timeout path breaks without draining).
    while let Ok(msg) = acp_rx.try_recv() {
        handle_headless_acp_message(
            msg.boxed(),
            &mut emitter,
            t_prompt,
            &mut ttf_logged,
            options.yolo,
            options.output_format,
            &mut pending_bg,
            &mut completed_before_bg,
        );
    }

    // Kill background tasks/subagents still pending at exit (background-wait
    // timeout or --no-wait-for-background) so they don't outlive the process.
    if !pending_bg.is_empty() {
        tracing::warn!(
            pending_bg = pending_bg.len(),
            "headless: killing background work still pending at exit"
        );
        reap_pending_background_tasks(&pending_bg, &session_id, &acp_tx).await;
    }

    // Flush buffered unified log entries before exit.
    crate::unified_log::flush_blocking().await;

    // Handle result
    if track_active {
        // Non-blocking flock so a slow/network ~/.grok can't hang exit.
        let _ = xai_grok_shell::active_sessions::try_unregister(&session_id);
    }
    cancel.cancel();
    match prompt_result {
        Some(Ok(resp)) => {
            let stop_reason = format!("{:?}", resp.stop_reason);
            emitter.set_structured_output_from_meta(resp.meta.as_ref());
            emitter.set_usage_from_meta(resp.meta.as_ref());
            let sid = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("sessionId"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let rid = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let is_max_turns = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("cancellationCategory"))
                .and_then(|v| v.as_str())
                == Some("max_turns_reached");
            if is_max_turns {
                match emitter.format {
                    OutputFormat::Plain => eprintln!("Max turns reached"),
                    OutputFormat::StreamingJson => {
                        println!("{}", serde_json::json!({"type": "max_turns_reached"}))
                    }
                    OutputFormat::Json => {} // conveyed by stopReason in the final JSON
                }
                emitter.on_end(&stop_reason, sid, rid);
                anyhow::bail!("max turns reached");
            }
            emitter.on_end(&stop_reason, sid, rid);
            Ok(())
        }
        Some(Err(err)) => {
            let msg = if i32::from(err.code) == RATE_LIMITED_ERROR_CODE {
                let detail = err.data.as_ref().and_then(error_detail_from_data);
                crate::app::sanitize_user_error(&format_rate_limited_user_message(
                    detail.as_deref(),
                    is_api_key_auth,
                ))
            } else {
                err.to_string()
            };
            if let Some(usage) = xai_grok_shell::sampling::error::prompt_usage_from_error(&err)
                && let Ok(v) = serde_json::to_value(&usage)
            {
                emitter.usage = Some(v);
            }
            emitter.on_error(&msg);
            anyhow::bail!("{msg}")
        }
        None => Ok(()),
    }
}

/// Ext request that kills pending background work `key` (a `pending_bg`
/// entry): `subagent:{id}` cancels the subagent, anything else kills the
/// bash/monitor task with that id.
fn reap_request_for_key(
    key: &str,
    session_id: &acp::SessionId,
) -> serde_json::Result<acp::ExtRequest> {
    let (method, params) = match key.strip_prefix("subagent:") {
        Some(id) => (
            "x.ai/subagent/cancel",
            serde_json::value::to_raw_value(&CancelSubagentRequest {
                subagent_id: id.to_string(),
            })?,
        ),
        None => (
            "x.ai/task/kill",
            serde_json::value::to_raw_value(&KillTaskRequest {
                session_id: session_id.0.to_string(),
                task_id: key.to_string(),
            })?,
        ),
    };
    Ok(acp::ExtRequest::new(method, params.into()))
}

/// Best-effort kill of background work still pending when headless exits
/// (background-wait timeout or `--no-wait-for-background`) so model-spawned
/// processes never outlive the process. Failures are logged, never fatal.
async fn reap_pending_background_tasks(
    pending_bg: &HashSet<String>,
    session_id: &acp::SessionId,
    acp_tx: &AcpAgentTx,
) {
    for key in pending_bg {
        let request = match reap_request_for_key(key, session_id) {
            Ok(request) => request,
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "headless: failed to build reap request");
                continue;
            }
        };
        let method = request.method.clone();
        match tokio::time::timeout(Duration::from_secs(10), acp_send(request, acp_tx)).await {
            Ok(Ok(_)) => {
                tracing::debug!(key = %key, %method, "headless: reaped pending background work")
            }
            Ok(Err(e)) => {
                tracing::warn!(key = %key, %method, error = %e, "headless: failed to reap background work")
            }
            Err(_) => {
                tracing::warn!(key = %key, %method, "headless: timed out reaping background work")
            }
        }
    }
}

/// Track a background lifecycle event in the pending set.
///
/// Tracking is unconditional — independent of `--no-wait-for-background` — so
/// the exit reaper sees everything still running. `wait_for_background` only
/// gates whether the loop waits for this set to drain.
fn track_background_lifecycle(
    event: ExtEvent,
    pending_bg: &mut HashSet<String>,
    completed_before_bg: &mut HashSet<String>,
) {
    match event {
        ExtEvent::TaskBackgrounded {
            task_id,
            is_monitor,
        } => {
            if !completed_before_bg.remove(&task_id) {
                pending_bg.insert(task_id);
                tracing::debug!(
                    pending = pending_bg.len(),
                    is_monitor,
                    "headless: tracking background task"
                );
            }
        }
        ExtEvent::TaskCompleted { task_id } => {
            if pending_bg.remove(&task_id) {
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: background task completed"
                );
            } else {
                completed_before_bg.insert(task_id);
            }
        }
        ExtEvent::SubagentSpawned { subagent_id } => {
            let key = format!("subagent:{subagent_id}");
            if !completed_before_bg.remove(&key) {
                pending_bg.insert(key);
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: tracking background subagent"
                );
            }
        }
        ExtEvent::SubagentFinished { subagent_id } => {
            let key = format!("subagent:{subagent_id}");
            if pending_bg.remove(&key) {
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: background subagent finished"
                );
            } else {
                completed_before_bg.insert(key);
            }
        }
        ExtEvent::MonitorEvent | ExtEvent::None => {}
    }
}

// ── ACP client message handling (select arm + pre-exit drain) ────────────

#[allow(clippy::too_many_arguments)]
async fn drain_acp_with_grace(
    acp_rx: &mut AcpClientRx,
    grace: Duration,
    emitter: &mut HeadlessEmitter,
    t_prompt: Instant,
    ttf_logged: &mut bool,
    yolo: bool,
    output_format: OutputFormat,
    pending_bg: &mut HashSet<String>,
    completed_before_bg: &mut HashSet<String>,
) {
    let deadline = Instant::now() + grace;
    loop {
        while let Ok(msg) = acp_rx.try_recv() {
            handle_headless_acp_message(
                msg.boxed(),
                emitter,
                t_prompt,
                ttf_logged,
                yolo,
                output_format,
                pending_bg,
                completed_before_bg,
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            biased;
            msg = acp_rx.recv() => {
                let Some(msg) = msg else { break; };
                handle_headless_acp_message(
                    msg.boxed(),
                    emitter,
                    t_prompt,
                    ttf_logged,
                    yolo,
                    output_format,
                    pending_bg,
                    completed_before_bg,
                );
            }
            _ = tokio::time::sleep(remaining) => {
                break;
            }
        }
    }
}

/// Process one inbound ACP client message. Used by both `acp_rx.recv()` and
/// `try_recv()` so buffered `task_backgrounded` is not dropped when
/// `PromptResponse` completes first.
#[allow(clippy::too_many_arguments)]
fn handle_headless_acp_message(
    msg: AcpClientMessageBox,
    emitter: &mut HeadlessEmitter,
    t_prompt: Instant,
    ttf_logged: &mut bool,
    yolo: bool,
    output_format: OutputFormat,
    pending_bg: &mut HashSet<String>,
    completed_before_bg: &mut HashSet<String>,
) {
    match msg {
        AcpClientMessageBox::SessionNotification(boxed) => {
            match &boxed.request.update {
                acp::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let acp::ContentBlock::Text(text) = &chunk.content
                        && !text.text.is_empty()
                    {
                        if !*ttf_logged {
                            *ttf_logged = true;
                            tracing::debug!(
                                elapsed_ms = t_prompt.elapsed().as_millis() as u64,
                                "headless: time-to-first-chunk"
                            );
                        }
                        emitter.on_text_chunk(&text.text);
                    }
                }
                acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                    if let acp::ContentBlock::Text(text) = &chunk.content {
                        if !*ttf_logged {
                            *ttf_logged = true;
                            tracing::debug!(
                                elapsed_ms = t_prompt.elapsed().as_millis() as u64,
                                "headless: time-to-first-thought"
                            );
                        }
                        emitter.on_thought_chunk(&text.text);
                    }
                }
                _ => {}
            }
            let _ = boxed.response_tx.send(Ok(()));
        }
        AcpClientMessageBox::RequestPermission(req) => {
            if yolo {
                if let Some(resp) = auto_respond_to_permissions(
                    &req.request,
                    &[
                        acp::PermissionOptionKind::AllowOnce,
                        acp::PermissionOptionKind::AllowAlways,
                    ],
                ) {
                    let _ = req.response_tx.send(Ok(resp));
                } else {
                    let _ = req.response_tx.send(Ok(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    )));
                }
            } else {
                let _ = req.response_tx.send(Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                )));
            }
        }
        AcpClientMessageBox::ExtNotification(notif) => {
            let event = handle_ext_notification(&notif, output_format);
            let _ = notif.response_tx.send(Ok(()));
            track_background_lifecycle(event, pending_bg, completed_before_bg);
        }
        AcpClientMessageBox::WaitForTerminalExit(args) => {
            args.response_tx
                .send(Err(crate::acp::wait_for_exit_not_supported(
                    "headless mode",
                )))
                .ok();
        }
        _ => {}
    }
}

// ── Extension notification handling ──────────────────────────────────────

enum ExtEvent {
    None,
    TaskBackgrounded {
        task_id: String,
        is_monitor: bool,
    },
    TaskCompleted {
        task_id: String,
    },
    SubagentSpawned {
        subagent_id: String,
    },
    SubagentFinished {
        subagent_id: String,
    },
    /// Monitor emitted a line (or ended streaming). Does not complete the task;
    /// completion still arrives via `TaskCompleted`.
    MonitorEvent,
}

fn handle_ext_notification(
    notif: &xai_acp_lib::AcpArgsBox<acp::ExtNotification>,
    format: OutputFormat,
) -> ExtEvent {
    let method = notif.request.method.as_ref();

    // Background task lifecycle uses dedicated methods (not session_notification).
    if method == "x.ai/task_backgrounded" {
        #[derive(serde::Deserialize)]
        struct TaskBgEnvelope {
            update: TaskBgUpdate,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
        enum TaskBgUpdate {
            TaskBackgrounded {
                task_id: String,
                #[serde(default)]
                monitor_description: Option<String>,
            },
            #[serde(other)]
            Other,
        }
        if let Ok(env) = serde_json::from_str::<TaskBgEnvelope>(notif.request.params.get())
            && let TaskBgUpdate::TaskBackgrounded {
                task_id,
                monitor_description,
            } = env.update
        {
            return ExtEvent::TaskBackgrounded {
                task_id,
                is_monitor: monitor_description.is_some(),
            };
        }
        return ExtEvent::None;
    }

    if method == "x.ai/task_completed" {
        #[derive(serde::Deserialize)]
        struct TaskDoneEnvelope {
            update: TaskDoneUpdate,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
        enum TaskDoneUpdate {
            TaskCompleted {
                task_snapshot: TaskSnapshotLite,
            },
            #[serde(other)]
            Other,
        }
        #[derive(serde::Deserialize)]
        struct TaskSnapshotLite {
            task_id: String,
        }
        if let Ok(env) = serde_json::from_str::<TaskDoneEnvelope>(notif.request.params.get())
            && let TaskDoneUpdate::TaskCompleted { task_snapshot } = env.update
        {
            return ExtEvent::TaskCompleted {
                task_id: task_snapshot.task_id,
            };
        }
        return ExtEvent::None;
    }

    if method == "x.ai/monitor_event" {
        return ExtEvent::MonitorEvent;
    }

    match method {
        "x.ai/session_notification" | "x.ai/session/update" => {}
        _ => return ExtEvent::None,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
    enum XaiUpdate {
        AutoCompactStarted {
            percentage: u8,
        },
        AutoCompactCompleted {},
        AutoCompactFailed {
            error: String,
        },
        AutoCompactCancelled {},
        AutoContinueCompleted {
            total_tokens: u64,
        },
        ImageCompressed {
            message: String,
        },
        SubagentSpawned {
            subagent_id: String,
        },
        SubagentFinished {
            subagent_id: String,
        },
        #[serde(other)]
        Other,
    }
    #[derive(serde::Deserialize)]
    struct XaiNotif {
        update: XaiUpdate,
    }

    let Ok(xai_notif) = serde_json::from_str::<XaiNotif>(notif.request.params.get()) else {
        return ExtEvent::None;
    };

    match xai_notif.update {
        XaiUpdate::AutoCompactStarted { percentage } => match format {
            OutputFormat::StreamingJson => {
                println!(
                    "{}",
                    serde_json::json!({"type": "auto_compact_started", "percentage": percentage})
                );
            }
            OutputFormat::Plain => {
                eprintln!("Auto-compacting conversation ({percentage}% full)...");
            }
            OutputFormat::Json => {}
        },
        XaiUpdate::AutoCompactCompleted {} => match format {
            OutputFormat::StreamingJson => {
                println!("{}", serde_json::json!({"type": "auto_compact_completed"}));
            }
            OutputFormat::Plain => eprintln!("Conversation compacted."),
            OutputFormat::Json => {}
        },
        XaiUpdate::AutoCompactFailed { error } => match format {
            OutputFormat::StreamingJson => {
                println!(
                    "{}",
                    serde_json::json!({"type": "auto_compact_failed", "error": error})
                );
            }
            OutputFormat::Plain => {
                if error.trim().is_empty() {
                    eprintln!("Auto-compact failed.");
                } else {
                    eprintln!("Auto-compact failed: {error}");
                }
            }
            OutputFormat::Json => {}
        },
        XaiUpdate::AutoCompactCancelled {} => match format {
            OutputFormat::StreamingJson => {
                println!("{}", serde_json::json!({"type": "auto_compact_cancelled"}));
            }
            OutputFormat::Plain => eprintln!("Auto-compact cancelled."),
            OutputFormat::Json => {}
        },
        XaiUpdate::AutoContinueCompleted { total_tokens } => match format {
            OutputFormat::StreamingJson => {
                println!(
                    "{}",
                    serde_json::json!({"type": "auto_continue_completed", "total_tokens": total_tokens})
                );
            }
            OutputFormat::Plain => eprintln!("Resumed after compaction."),
            OutputFormat::Json => {}
        },
        XaiUpdate::ImageCompressed { message } => match format {
            OutputFormat::StreamingJson => {
                println!(
                    "{}",
                    serde_json::json!({"type": "image_compressed", "message": message})
                );
            }
            OutputFormat::Plain => eprintln!("{message}"),
            OutputFormat::Json => {}
        },
        XaiUpdate::SubagentSpawned { subagent_id } => {
            return ExtEvent::SubagentSpawned { subagent_id };
        }
        XaiUpdate::SubagentFinished { subagent_id, .. } => {
            return ExtEvent::SubagentFinished { subagent_id };
        }
        XaiUpdate::Other => {}
    }
    ExtEvent::None
}

#[cfg(test)]
mod tests {
    #[test]
    fn lifecycle_tracking_is_independent_of_wait_flag() {
        let mut pending = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        super::track_background_lifecycle(
            super::ExtEvent::TaskBackgrounded {
                task_id: "t1".into(),
                is_monitor: false,
            },
            &mut pending,
            &mut completed,
        );
        super::track_background_lifecycle(
            super::ExtEvent::SubagentSpawned {
                subagent_id: "s1".into(),
            },
            &mut pending,
            &mut completed,
        );
        assert!(pending.contains("t1"));
        assert!(pending.contains("subagent:s1"));

        super::track_background_lifecycle(
            super::ExtEvent::TaskCompleted {
                task_id: "t1".into(),
            },
            &mut pending,
            &mut completed,
        );
        super::track_background_lifecycle(
            super::ExtEvent::SubagentFinished {
                subagent_id: "s1".into(),
            },
            &mut pending,
            &mut completed,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn completion_before_backgrounded_never_rearms_pending() {
        let mut pending = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        super::track_background_lifecycle(
            super::ExtEvent::TaskCompleted {
                task_id: "t1".into(),
            },
            &mut pending,
            &mut completed,
        );
        super::track_background_lifecycle(
            super::ExtEvent::TaskBackgrounded {
                task_id: "t1".into(),
                is_monitor: false,
            },
            &mut pending,
            &mut completed,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn reap_request_for_task_kills_with_session_scope() {
        let session_id = acp::SessionId::new("sess-1");
        let request = super::reap_request_for_key("task-42", &session_id).unwrap();
        assert_eq!(request.method.as_ref(), "x.ai/task/kill");
        let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
        assert_eq!(params["sessionId"], "sess-1");
        assert_eq!(params["taskId"], "task-42");
    }

    #[test]
    fn reap_request_for_subagent_cancels_with_stripped_id() {
        let session_id = acp::SessionId::new("sess-1");
        let request = super::reap_request_for_key("subagent:sub-7", &session_id).unwrap();
        assert_eq!(request.method.as_ref(), "x.ai/subagent/cancel");
        let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
        assert_eq!(params["subagentId"], "sub-7");
    }

    use super::*;
    use xai_grok_workspace::permission::types::{RuleAction, ToolFilter};

    fn s(v: &str) -> String {
        v.to_owned()
    }

    /// Headless materialization is never chat, regardless of worktree flag —
    /// resume targets stay disk/GCS Build sessions.
    #[test]
    fn headless_materialize_ctx_stays_non_chat() {
        for has_worktree in [false, true] {
            let ctx = headless_materialize_ctx(has_worktree);
            assert!(!ctx.chat_mode);
            assert!(!ctx.allow_remote_restore);
            assert_eq!(ctx.has_worktree, has_worktree);
        }
    }

    #[test]
    fn strict_valid_rules_parse_deny_before_allow() {
        let allow = vec![s("Bash(npm*)")];
        let deny = vec![s("Bash(rm*)"), s("Edit(/etc/**)")];
        let rules = parse_permission_rules_strict(&allow, &deny).unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].action, RuleAction::Deny);
        assert!(matches!(rules[0].tool, ToolFilter::Bash));
        assert_eq!(rules[1].action, RuleAction::Deny);
        assert!(matches!(rules[1].tool, ToolFilter::Edit));
        assert_eq!(rules[2].action, RuleAction::Allow);
        assert!(matches!(rules[2].tool, ToolFilter::Bash));
    }

    #[test]
    fn strict_invalid_rule_errors() {
        let result = parse_permission_rules_strict(&[], &[s("EnterWorktree(foo)")]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("--deny"));
        assert!(msg.contains("EnterWorktree"));
    }

    #[test]
    fn strict_reports_all_invalid_rules() {
        let result = parse_permission_rules_strict(
            &[s("BadTool(x)")],
            &[s("EnterWorktree(foo)"), s("Bash(rm*)")],
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EnterWorktree"),
            "should mention first bad deny"
        );
        assert!(msg.contains("BadTool"), "should mention bad allow");
    }

    #[test]
    fn lenient_skips_invalid_keeps_valid() {
        let allow = vec![s("Bash(npm*)")];
        let deny = vec![s("EnterWorktree(foo)"), s("Bash(rm*)")];
        let rules = parse_permission_rules_lenient(&allow, &deny);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].action, RuleAction::Deny);
        assert_eq!(rules[0].pattern.as_deref(), Some("rm*"));
        assert_eq!(rules[1].action, RuleAction::Allow);
        assert_eq!(rules[1].pattern.as_deref(), Some("npm*"));
    }

    #[test]
    fn empty_inputs_produce_empty_rules() {
        let rules = parse_permission_rules_strict(&[], &[]).unwrap();
        assert!(rules.is_empty());
        let rules = parse_permission_rules_lenient(&[], &[]);
        assert!(rules.is_empty());
    }

    #[test]
    fn domain_mode_web_fetch() {
        let rules = parse_permission_rules_strict(&[], &[s("WebFetch(domain:evil.com)")]).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(matches!(rules[0].tool, ToolFilter::WebFetch));
        assert_eq!(
            rules[0].pattern_mode,
            xai_grok_workspace::permission::types::PatternMode::Domain
        );
        assert_eq!(rules[0].pattern.as_deref(), Some("evil.com"));
    }

    #[test]
    fn bash_colon_wildcard_deny_translates_to_prefix() {
        let rules = parse_permission_rules_strict(&[], &[s("Bash(sed:*)")]).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(matches!(rules[0].tool, ToolFilter::Bash));
        assert_eq!(rules[0].pattern.as_deref(), Some("sed"));
    }

    #[test]
    fn structured_output_without_meta_errors_never_parses_text() {
        // No `_meta` structured output (e.g. max-turns/cancel): emit a clean
        // error, never an unvalidated parse of the raw text buffer.
        let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
        emitter.text_buffer = r#"{"name":"alice","age":30}"#.into();
        emitter.set_structured_output_from_meta(serde_json::json!({}).as_object());
        let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
        assert!(result["structuredOutput"].is_null());
        assert_eq!(
            result["structuredOutputError"],
            "model did not produce structured output"
        );
    }

    #[test]
    fn structured_output_from_meta_wins_over_text_buffer() {
        // The agent's validated output (from `_meta`) must override accumulated
        // prose (the multi-round corruption bug).
        let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
        emitter.text_buffer = "thinking out loud...".into();
        emitter.set_structured_output_from_meta(
            serde_json::json!({"structuredOutput": {"name": "carol"}}).as_object(),
        );
        let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
        assert_eq!(result["structuredOutput"]["name"], "carol");
        assert!(result.get("structuredOutputError").is_none());

        let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
        emitter.set_structured_output_from_meta(
            serde_json::json!({
                "structuredOutputError": "output does not match the required schema"
            })
            .as_object(),
        );
        let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
        assert!(result["structuredOutput"].is_null());
        assert_eq!(
            result["structuredOutputError"],
            "output does not match the required schema"
        );
    }

    #[test]
    fn streaming_json_structured_output_emits_from_meta() {
        let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingJson, true);
        emitter.on_text_chunk(r#"{"name":"#);
        emitter.on_text_chunk(r#""bob"}"#);
        assert_eq!(emitter.text_buffer, r#"{"name":"bob"}"#);

        // structuredOutput comes from the prompt-response `_meta`, not the buffer.
        emitter.set_structured_output_from_meta(
            serde_json::json!({"structuredOutput": {"name": "bob"}}).as_object(),
        );
        let mut target = serde_json::json!({});
        emitter.attach_structured_output(&mut target);
        assert_eq!(target["structuredOutput"]["name"], "bob");
        assert!(target.get("structuredOutputError").is_none());
    }

    #[test]
    fn parse_json_schema_rejects_non_objects_and_invalid_json() {
        assert!(super::parse_json_schema(r#"{"type":"object"}"#).is_ok());
        assert!(
            super::parse_json_schema(r#"[1,2,3]"#)
                .unwrap_err()
                .to_string()
                .contains("must be a JSON object")
        );
        assert!(
            super::parse_json_schema(r#"{not json"#)
                .unwrap_err()
                .to_string()
                .contains("invalid JSON")
        );
    }

    fn make_ext_notif(
        method: &str,
        update: serde_json::Value,
    ) -> xai_acp_lib::AcpArgsBox<acp::ExtNotification> {
        let payload = serde_json::json!({
            "sessionId": "sess-1",
            "update": update,
        });
        let raw = serde_json::value::to_raw_value(&payload).unwrap();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        xai_acp_lib::AcpArgs {
            request: acp::ExtNotification::new(method, raw.into()),
            response_tx: tx,
        }
        .boxed()
    }

    #[test]
    fn headless_task_backgrounded_parses_task_id() {
        // `make_ext_notif` wraps the arg under `update`, so pass
        // the inner update object (matching the real `x.ai/task_backgrounded`
        // wire shape: `{ "update": { "sessionUpdate": ..., "task_id": ... } }`).
        let notif = make_ext_notif(
            "x.ai/task_backgrounded",
            serde_json::json!({
                "sessionUpdate": "task_backgrounded",
                "task_id": "task-abc",
            }),
        );
        assert!(matches!(
                    handle_ext_notification(&notif, OutputFormat::Plain),
                    ExtEvent::TaskBackgrounded { task_id, is_monitor: false }
        if task_id == "task-abc"
                ));
    }

    #[test]
    fn headless_task_backgrounded_with_monitor_description_is_monitor() {
        let notif = make_ext_notif(
            "x.ai/task_backgrounded",
            serde_json::json!({
                "sessionUpdate": "task_backgrounded",
                "task_id": "mon-1",
                "monitor_description": "watching logs",
            }),
        );
        assert!(matches!(
                    handle_ext_notification(&notif, OutputFormat::Plain),
                    ExtEvent::TaskBackgrounded { task_id, is_monitor: true }
        if task_id == "mon-1"
                ));
    }

    #[test]
    fn headless_task_completed_parses_task_id() {
        // `task_completed` nests the id under `task_snapshot`. The
        // internally-tagged `rename_all = "snake_case"` renames only the
        // `sessionUpdate` tag, so `task_id` / `task_snapshot` stay snake_case;
        // this test guards against a future `rename_all = "camelCase"` on
        // `TaskSnapshot` silently turning waiting into a no-op.
        let notif = make_ext_notif(
            "x.ai/task_completed",
            serde_json::json!({
                "sessionUpdate": "task_completed",
                "task_snapshot": { "task_id": "task-abc" }
            }),
        );
        assert!(matches!(
                    handle_ext_notification(&notif, OutputFormat::Plain),
                    ExtEvent::TaskCompleted { task_id }
        if task_id == "task-abc"
                ));
    }

    #[test]
    fn headless_subagent_spawned_and_finished_parse() {
        let spawned = make_ext_notif(
            "x.ai/session_notification",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1",
                "parent_session_id": "p",
                "child_session_id": "c",
                "subagent_type": "explore",
                "description": "test"
            }),
        );
        assert!(matches!(
                    handle_ext_notification(&spawned, OutputFormat::Plain),
                    ExtEvent::SubagentSpawned { subagent_id }
        if subagent_id == "sub-1"
                ));
        let finished = make_ext_notif(
            "x.ai/session_notification",
            serde_json::json!({
                "sessionUpdate": "subagent_finished",
                "subagent_id": "sub-1",
                "child_session_id": "c",
                "status": "completed",
                "tool_calls": 0,
                "turns": 1,
                "duration_ms": 5
            }),
        );
        assert!(matches!(
                    handle_ext_notification(&finished, OutputFormat::Plain),
                    ExtEvent::SubagentFinished { subagent_id }
        if subagent_id == "sub-1"
                ));
    }

    #[test]
    fn headless_session_update_unknown_method_is_none() {
        let payload = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sub-1"
            }
        });
        let raw = serde_json::value::to_raw_value(&payload).unwrap();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let notif = xai_acp_lib::AcpArgs {
            request: acp::ExtNotification::new("x.ai/other", raw.into()),
            response_tx: tx,
        }
        .boxed();
        assert!(matches!(
            handle_ext_notification(&notif, OutputFormat::Plain),
            ExtEvent::None
        ));
    }
}
