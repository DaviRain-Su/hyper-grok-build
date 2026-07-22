//! Canonical session-selection CLI intent.
//!
//! Built once from CLI flags and consumed by interactive resolve, the event
//! loop, and headless mode so resume / new-with-id / fork are not re-derived
//! in three places.
use super::cli::PagerArgs;
use std::path::{Path, PathBuf};
/// Session-create intent deferred until [`AppView::session_startup_allowed`].
///
/// Replaces the prior matrix of `startup_load_session` + cwd + `startup_fork`
/// tuple + ad-hoc preferred-only replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredSessionStartup {
    /// Strict resume (`-r` / `-c` / picker load).
    Load {
        session_id: String,
        session_cwd: Option<PathBuf>,
        /// Conversation-entry bit (`source == "conversation"`), not sticky `--chat`.
        chat_kind: bool,
    },
    /// Client-chosen id (`--session-id`); also stashes preferred for picker.
    NewWithId { session_id: String },
    /// Startup `--fork-session` after parent resolve.
    Fork {
        parent_session_id: String,
        parent_cwd: Option<PathBuf>,
        new_session_id: Option<String>,
    },
    /// Fresh plain Grok session whose first prompt resumes a foreign tool session.
    ForeignResume {
        tool: xai_grok_workspace::foreign_sessions::ForeignSessionTool,
        native_id: String,
    },
}
/// One owner for every action deferred behind auth/folder-trust startup gates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeferredStartupActions {
    pub session: Option<DeferredSessionStartup>,
    pub preferred_session_id: Option<String>,
    pub worktree: bool,
    pub worktree_label: Option<String>,
    pub worktree_ref: Option<String>,
    pub new_session: bool,
    pub prompt: Option<String>,
    pub open_dashboard: bool,
    pub pending_chat: bool,
}
impl DeferredStartupActions {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}
/// Build `x.ai/session/fork` params shared by TUI effects and headless.
///
/// `new_cwd` is the write namespace for the child (parent session cwd when
/// cross-cwd); preflight must use the same path via [`effective_fork_new_cwd`].
pub fn fork_session_params(
    parent_session_id: &str,
    parent_cwd: &Path,
    new_session_id: Option<&str>,
    parent_is_worktree: bool,
) -> serde_json::Value {
    let parent_cwd_str = parent_cwd.to_string_lossy().into_owned();
    let source_cwd = xai_grok_shell::session::resolve_local_session_any_cwd(parent_session_id)
        .unwrap_or_else(|| parent_cwd_str.clone());
    let mut payload = serde_json::json!(
        { "sourceSessionId" : parent_session_id, "sourceCwd" : source_cwd, "newCwd" :
        parent_cwd_str.clone(), "sessionKind" : "fork", }
    );
    if let Some(nid) = new_session_id {
        payload["newSessionId"] = serde_json::Value::String(nid.to_string());
    }
    if parent_is_worktree {
        payload["sourceWorkspaceDir"] = serde_json::Value::String(parent_cwd_str);
    }
    payload
}
/// Whether a persisted session (or its cwd) is worktree-backed.
/// Mirrors in-session `/fork` reading `agent.session.is_worktree`.
pub fn parent_session_is_worktree(session_id: &str, cwd: &Path) -> bool {
    let cwd_str = cwd.to_string_lossy();
    let sessions_root = xai_grok_shell::util::grok_home::grok_home().join("sessions");
    let encoded = xai_grok_shell::util::grok_home::encode_cwd_dirname(&cwd_str);
    let summary_path = sessions_root
        .join(encoded)
        .join(session_id)
        .join("summary.json");
    if let Ok(bytes) = std::fs::read(&summary_path)
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        if v.get("session_kind").and_then(|k| k.as_str()) == Some("worktree") {
            return true;
        }
        if v.get("source_workspace_dir")
            .and_then(|k| k.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            return true;
        }
    }
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        let git = dir.join(".git");
        if git.is_file() {
            return true;
        }
        if git.is_dir() {
            return false;
        }
        cur = dir.parent();
    }
    false
}
/// Parse `newSessionId` from an `x.ai/session/fork` ACP response body.
pub fn fork_response_new_session_id(resp_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(resp_json).unwrap_or_default();
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return None;
    }
    v.get("newSessionId")
        .and_then(|x| x.as_str())
        .or_else(|| {
            v.get("result")
                .and_then(|r| r.get("newSessionId"))
                .and_then(|x| x.as_str())
        })
        .map(|s| s.to_string())
}
/// Error string from a fork response, if present.
pub fn fork_response_error(resp_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(resp_json).ok()?;
    v.get("error")
        .filter(|e| !e.is_null())
        .map(|e| e.to_string())
}
/// Pure interpretation of session-selection CLI flags (no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStartupIntent {
    /// Fresh session; agent picks the ID.
    NewAuto,
    /// Fresh session with a client-chosen ID (must not exist under cwd).
    NewWithId { session_id: String },
    /// Load an existing session (strict — never create).
    Resume {
        /// `None` means resolve most-recent for cwd at materialize time.
        session_id: Option<String>,
        most_recent_for_cwd: bool,
    },
    /// Resolve source like resume, then fork; optional forced ID for the child.
    ForkFrom {
        source_session_id: Option<String>,
        most_recent_for_cwd: bool,
        new_session_id: Option<String>,
    },
}
/// Flag combinations that clap allows but we reject at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupFlagError {
    /// `--session-id` with resume/continue/load without `--fork-session`.
    SessionIdRequiresFork,
    /// `--fork-session` without resume/continue/load.
    ForkRequiresResumeOrContinue,
    /// `--fork-session` with `--worktree` (not supported yet).
    ForkWithWorktree,
}
impl std::fmt::Display for StartupFlagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionIdRequiresFork => {
                write!(
                    f,
                    "Error: --session-id can only be used with --continue or --resume if --fork-session is also specified."
                )
            }
            Self::ForkRequiresResumeOrContinue => {
                write!(f, "Error: --fork-session requires --resume or --continue.")
            }
            Self::ForkWithWorktree => {
                write!(
                    f,
                    "Error: --fork-session cannot be combined with --worktree."
                )
            }
        }
    }
}
impl std::error::Error for StartupFlagError {}
/// Inputs shared by interactive CLI and headless (no clap dependency).
#[derive(Debug, Clone, Copy)]
pub struct SessionStartupFlags<'a> {
    pub session_id: Option<&'a str>,
    /// Explicit resume id from `-r` / `--resume` (not the empty most-recent sentinel).
    pub resume_session_id: Option<&'a str>,
    /// `--resume` with no value (most recent for cwd).
    pub resume_most_recent: bool,
    pub continue_last_session: bool,
    pub fork_session: bool,
    /// True when `--worktree` is set (any label, including empty default).
    pub has_worktree: bool,
}
/// Classify session-selection flags into a single intent (no I/O).
pub fn session_startup_intent_from_flags(
    f: SessionStartupFlags<'_>,
) -> Result<SessionStartupIntent, StartupFlagError> {
    let has_resume_id = f.resume_session_id.is_some();
    let most_recent = f.resume_most_recent || f.continue_last_session;
    let has_resume_or_continue = has_resume_id || most_recent;
    if f.fork_session && f.has_worktree {
        return Err(StartupFlagError::ForkWithWorktree);
    }
    if f.fork_session && !has_resume_or_continue {
        return Err(StartupFlagError::ForkRequiresResumeOrContinue);
    }
    if let Some(sid) = f.session_id {
        if has_resume_or_continue && !f.fork_session {
            return Err(StartupFlagError::SessionIdRequiresFork);
        }
        if f.fork_session {
            return Ok(SessionStartupIntent::ForkFrom {
                source_session_id: f.resume_session_id.map(|s| s.to_owned()),
                most_recent_for_cwd: most_recent && !has_resume_id,
                new_session_id: Some(sid.to_owned()),
            });
        }
        return Ok(SessionStartupIntent::NewWithId {
            session_id: sid.to_owned(),
        });
    }
    if f.fork_session {
        return Ok(SessionStartupIntent::ForkFrom {
            source_session_id: f.resume_session_id.map(|s| s.to_owned()),
            most_recent_for_cwd: most_recent && !has_resume_id,
            new_session_id: None,
        });
    }
    if let Some(id) = f.resume_session_id {
        return Ok(SessionStartupIntent::Resume {
            session_id: Some(id.to_owned()),
            most_recent_for_cwd: false,
        });
    }
    if most_recent {
        return Ok(SessionStartupIntent::Resume {
            session_id: None,
            most_recent_for_cwd: true,
        });
    }
    Ok(SessionStartupIntent::NewAuto)
}
impl PagerArgs {
    /// Classify session-selection flags into a single intent (no I/O).
    pub fn session_startup_intent(&self) -> Result<SessionStartupIntent, StartupFlagError> {
        session_startup_intent_from_flags(SessionStartupFlags {
            session_id: self.session_id.as_deref(),
            resume_session_id: self.session_to_resume(),
            resume_most_recent: self.resume_most_recent(),
            continue_last_session: self.continue_last_session,
            fork_session: self.fork_session,
            has_worktree: self.worktree.is_some(),
        })
    }
}
/// User-facing refusal when process-wide `--chat` would open a local Build disk row.
pub const CHAT_MODE_LOCAL_BUILD_REFUSAL: &str = "cannot open a local Build session while --chat is active; \
resume a conversation or start a new chat (/chat)";
/// User-facing error when `--chat` is combined with leader mode.
pub const CHAT_MODE_LEADER_CONFLICT: &str = "gateway chat mode (--chat) cannot run with leader mode; \
pass --no-leader or disable [cli] use_leader in config";
/// Startup guard used by TUI `run` (and unit-tested): sticky `--chat` + leader is invalid.
#[inline]
pub fn chat_mode_conflicts_with_leader(chat: bool, use_leader: bool) -> bool {
    chat && use_leader
}
/// User-facing error for `--fork-session` + `--chat` (forking is a Build disk
/// concept; chat sessions have no local copy to fork).
pub const CHAT_MODE_FORK_CONFLICT: &str = "--fork-session is not supported with --chat";
/// User-facing error for `--restore-code` + `--chat` (code restore is a
/// Build/worktree concept; chat sessions carry no codebase).
pub const CHAT_MODE_RESTORE_CODE_CONFLICT: &str = "--restore-code is not supported with --chat";
/// Flag validation: Build-lifecycle flags that cannot combine with `--chat`.
/// Always `None` when `chat_mode` is false, so call sites need no `cfg`.
pub fn chat_mode_flag_conflict(
    chat_mode: bool,
    fork_session: bool,
    restore_code: bool,
) -> Option<&'static str> {
    if !chat_mode {
        return None;
    }
    if fork_session {
        return Some(CHAT_MODE_FORK_CONFLICT);
    }
    if restore_code {
        return Some(CHAT_MODE_RESTORE_CODE_CONFLICT);
    }
    None
}
/// Conservative shape check for a chat-mode `--resume <id>` passthrough.
///
/// The id skips disk/GCS resolution and flows to the gateway, but it is also
/// path-joined by the local cwd-collision check — so reject path separators,
/// dots, and anything outside the conversation-id alphabet before it leaves
/// materialization. Existence is still validated by the gateway at load.
pub fn valid_conversation_id_shape(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}
/// True when `session_id` resolves under the **cwd-scoped** local Build sessions
/// tree. Deliberately does **not** use `resolve_local_session_any_cwd`: a gateway
/// conversation id that collides with a Build session under another cwd must not
/// false-refuse CLI resume / non-entry loads under `--chat`.
pub fn local_build_session_on_disk(session_id: &str, cwd: &Path) -> bool {
    let cwd_str = cwd.to_string_lossy();
    xai_grok_shell::session::resolve_local_session(session_id, &cwd_str).is_some()
}
/// Pure policy: process-wide `--chat` refuses a local Build disk row unless the
/// caller marked an explicit conversation entry (picker `source == "conversation"`).
pub fn chat_mode_refuses_local_build(
    chat_mode: bool,
    conversation_entry: bool,
    is_local_build_on_disk: bool,
) -> bool {
    chat_mode && !conversation_entry && is_local_build_on_disk
}
/// Process-wide `--chat` must not load (or coerce) local Build disk rows.
///
/// `conversation_entry` is true only for picker/list rows with
/// `source == "conversation"` (or restore that preserved that bit) — **not**
/// merely because sticky `--chat` / `chat_mode` is set.
///
/// Short-circuits before any disk walk when `--chat` is off or the row is a
/// conversation entry.
pub fn chat_mode_refuses_local_build_load(
    chat_mode: bool,
    conversation_entry: bool,
    session_id: &str,
    cwd: &Path,
) -> bool {
    if !chat_mode || conversation_entry {
        return false;
    }
    local_build_session_on_disk(session_id, cwd)
}
/// Outcome of async materialization (local resolve / remote restore / preflight).
#[derive(Debug, Clone)]
pub enum MaterializedStartup {
    /// Create a new session with an agent-chosen ID (or defer to welcome).
    NewAuto,
    /// Create a new session with this ID (`session/new` meta.sessionId).
    NewWithId { session_id: String },
    /// Strict load of an existing session.
    Resume {
        session_id: String,
        original_cwd: Option<PathBuf>,
        title: Option<String>,
    },
    /// Fork from a resolved parent, then load the child.
    Fork {
        parent_session_id: String,
        parent_cwd: Option<PathBuf>,
        parent_title: Option<String>,
        new_session_id: Option<String>,
    },
}
/// Context for [`materialize_startup`] (interactive vs headless share this).
#[derive(Debug, Clone, Copy)]
pub struct MaterializeCtx {
    /// When true, skip process-cwd preflight for `NewWithId` (worktree create
    /// checks the final session cwd later).
    pub has_worktree: bool,
    /// When true, attempt remote restore if the session is not on disk.
    pub allow_remote_restore: bool,
    /// Process-wide flag: resume targets are grok.com conversations, not
    /// the local disk store. Always `false` without the optional feature;
    /// setting it anyway errors rather than silently falling back to disk.
    pub chat_mode: bool,
}
impl MaterializeCtx {
    /// `--resume` miss bails fast.
    pub const fn default_allow_remote_restore() -> bool {
        false
    }
    pub fn from_pager_args(args: &PagerArgs) -> Self {
        Self {
            has_worktree: args.worktree.is_some(),
            allow_remote_restore: Self::default_allow_remote_restore(),
            chat_mode: args.chat(),
        }
    }
}
/// Cwd where a forked child session is written (interactive + headless SSOT).
///
/// When the parent lives under another directory, the fork effect sets
/// `newCwd` to that parent session cwd — preflight must use the same path.
pub fn effective_fork_new_cwd(process_cwd: &str, parent_cwd: Option<&Path>) -> String {
    parent_cwd
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| process_cwd.to_string())
}
/// Resolve most-recent session id for cwd, or error.
async fn most_recent_session_id(cwd: &str) -> anyhow::Result<(String, Option<String>)> {
    let summaries = xai_grok_shell::session::persistence::list_summaries(Some(cwd)).await?;
    let first = summaries.first().ok_or_else(|| {
        anyhow::anyhow!(
            "No session found for current directory. \
             Use 'grok' to start a new session."
        )
    })?;
    Ok((first.info.id.to_string(), first.display_title_opt()))
}
/// `AuthManager` for direct grok.com calls made outside the agent (pre-ACP
/// `--continue` conversation listing, the GCS restore effect). Wires the
/// auth-provider refresher before the first `auth()`: without it, environments
/// that mint credentials via `auth_provider_command` report `NoOauth`.
pub(crate) fn pre_acp_auth_manager(
    agent_config: &xai_grok_shell::agent::config::Config,
) -> std::sync::Arc<xai_grok_shell::auth::AuthManager> {
    let auth = std::sync::Arc::new(xai_grok_shell::auth::AuthManager::new(
        &xai_grok_shell::util::grok_home::grok_home(),
        agent_config.grok_com_config.clone(),
    ));
    auth.configure_refresher(
        agent_config.grok_com_config.auth_provider_command.clone(),
        None,
    );
    auth
}
/// Preflight: preferred id must be a UUID and not a persisted session under `cwd`.
///
/// Agent `session/new` rejects non-UUID `_meta.sessionId`; fail fast here so
/// CLI users get a clear error before ACP.
pub fn ensure_session_id_available(session_id: &str, cwd: &str) -> anyhow::Result<()> {
    if uuid::Uuid::try_parse(session_id).is_err() {
        anyhow::bail!("Error: --session-id must be a valid UUID (got '{session_id}').");
    }
    if xai_grok_shell::session::persistence::session_exists_for_cwd(session_id, cwd) {
        anyhow::bail!("Error: Session ID {session_id} is already in use.");
    }
    Ok(())
}
/// Materialize CLI intent into a concrete startup plan (I/O + remote restore).
pub async fn materialize_startup(
    ctx: MaterializeCtx,
    intent: SessionStartupIntent,
) -> anyhow::Result<MaterializedStartup> {
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get cwd: {e}"))?
        .to_string_lossy()
        .to_string();
    materialize_startup_for_cwd(ctx, intent, &cwd).await
}
/// Same as [`materialize_startup`] but with an explicit process cwd (tests / headless).
pub async fn materialize_startup_for_cwd(
    ctx: MaterializeCtx,
    intent: SessionStartupIntent,
    cwd: &str,
) -> anyhow::Result<MaterializedStartup> {
    if ctx.chat_mode && matches!(intent, SessionStartupIntent::ForkFrom { .. }) {
        anyhow::bail!("{CHAT_MODE_FORK_CONFLICT}");
    }
    match intent {
        SessionStartupIntent::NewAuto => Ok(MaterializedStartup::NewAuto),
        SessionStartupIntent::NewWithId { session_id } => {
            if !ctx.has_worktree {
                ensure_session_id_available(&session_id, cwd)?;
            } else if uuid::Uuid::try_parse(&session_id).is_err() {
                anyhow::bail!("Error: --session-id must be a valid UUID (got '{session_id}').");
            }
            Ok(MaterializedStartup::NewWithId { session_id })
        }
        SessionStartupIntent::Resume {
            session_id: None,
            most_recent_for_cwd: true,
        } => {
            if ctx.chat_mode {
                anyhow::bail!("chat-mode resume requires a build with the `chat` cargo feature");
            }
            let started = std::time::Instant::now();
            let (id, title) = most_recent_session_id(cwd).await?;
            tracing::info!(
                source = "local",
                elapsed_ms = started.elapsed().as_millis() as u64,
                "startup.continue.resolve"
            );
            Ok(MaterializedStartup::Resume {
                session_id: id,
                original_cwd: None,
                title,
            })
        }
        SessionStartupIntent::ForkFrom {
            source_session_id: None,
            most_recent_for_cwd: true,
            new_session_id,
        } => {
            if let Some(ref nid) = new_session_id {
                ensure_session_id_available(nid, cwd)?;
            }
            let (id, title) = most_recent_session_id(cwd).await?;
            Ok(MaterializedStartup::Fork {
                parent_session_id: id,
                parent_cwd: None,
                parent_title: title,
                new_session_id,
            })
        }
        SessionStartupIntent::Resume {
            session_id: Some(session_id),
            ..
        } => {
            if ctx.chat_mode {
                if !valid_conversation_id_shape(&session_id) {
                    anyhow::bail!("invalid conversation id {session_id:?}");
                }
                return Ok(MaterializedStartup::Resume {
                    session_id,
                    original_cwd: None,
                    title: None,
                });
            }
            let r = resolve_existing_session(ctx, &session_id, cwd).await?;
            Ok(MaterializedStartup::Resume {
                session_id: r.id,
                original_cwd: r.original_cwd,
                title: r.title,
            })
        }
        SessionStartupIntent::ForkFrom {
            source_session_id: Some(session_id),
            new_session_id,
            ..
        } => {
            let r = resolve_existing_session(ctx, &session_id, cwd).await?;
            if let Some(ref nid) = new_session_id {
                let new_cwd = effective_fork_new_cwd(cwd, r.original_cwd.as_deref());
                ensure_session_id_available(nid, &new_cwd)?;
            }
            Ok(MaterializedStartup::Fork {
                parent_session_id: r.id,
                parent_cwd: r.original_cwd,
                parent_title: r.title,
                new_session_id,
            })
        }
        SessionStartupIntent::Resume {
            session_id: None,
            most_recent_for_cwd: false,
        }
        | SessionStartupIntent::ForkFrom {
            source_session_id: None,
            most_recent_for_cwd: false,
            ..
        } => {
            anyhow::bail!("internal: invalid session startup intent (unreachable from CLI flags)")
        }
    }
}
struct ResolvedExisting {
    id: String,
    original_cwd: Option<PathBuf>,
    title: Option<String>,
}
/// Resolve an existing session for strict resume (local / any-cwd / remote / worktree defer).
async fn resolve_existing_session(
    ctx: MaterializeCtx,
    session_id: &str,
    cwd: &str,
) -> anyhow::Result<ResolvedExisting> {
    if let Some(local_id) = xai_grok_shell::session::resolve_local_session(session_id, cwd) {
        tracing::info!(
            session_id = % session_id, local_id = % local_id, "Session found locally"
        );
        return Ok(ResolvedExisting {
            id: local_id,
            original_cwd: None,
            title: None,
        });
    }
    if let Some(original_cwd) = xai_grok_shell::session::resolve_local_session_any_cwd(session_id) {
        tracing::info!(
            session_id = % session_id, original_cwd = % original_cwd,
            "Session found locally under different CWD"
        );
        eprintln!(
            "Session {} found locally (originally in {})",
            session_id, original_cwd
        );
        return Ok(ResolvedExisting {
            id: session_id.to_string(),
            original_cwd: Some(PathBuf::from(original_cwd)),
            title: None,
        });
    }
    if ctx.has_worktree {
        tracing::info!(
            session_id = % session_id,
            "Session not found locally; deferring restore to worktree resume handler"
        );
        eprintln!(
            "Session {} not found locally; it will be restored into the new worktree.",
            session_id
        );
        return Ok(ResolvedExisting {
            id: session_id.to_string(),
            original_cwd: None,
            title: None,
        });
    }
    if !ctx.allow_remote_restore {
        anyhow::bail!("Session does not exist");
    }
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    if let Some((false, source)) =
        xai_grok_shell::util::config::session_registry_local_override_sourced(Some(&raw_config))
    {
        anyhow::bail!(
            "Session does not exist locally (session registry is disabled by {})",
            source.label()
        );
    }
    eprintln!(
        "Session {} not found locally, restoring from remote...",
        session_id
    );
    let agent_config = xai_grok_shell::agent::config::Config::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {}", e))?;
    use xai_grok_shell::agent::session_registry_client::SessionRegistryClient;
    use xai_grok_shell::auth::{AuthManager, ensure_authenticated_or_noninteractive};
    use xai_grok_shell::session::restore::restore_session_with_storage;
    use xai_grok_shell::util::grok_home::grok_home;
    let deployment_key = agent_config.endpoints.deployment_key.clone();
    ensure_authenticated_or_noninteractive(
        &agent_config.grok_com_config,
        deployment_key.is_some(),
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to authenticate for session restore: {}", e))?;
    let auth_manager = std::sync::Arc::new(AuthManager::new(
        &grok_home(),
        agent_config.grok_com_config.clone(),
    ));
    let registry_client =
        SessionRegistryClient::new(agent_config.endpoints.proxy_url(), String::new())
            .with_deployment_key(deployment_key.clone())
            .with_alpha_test_key(agent_config.endpoints.alpha_test_key.clone())
            .with_auth(auth_manager.clone());
    let storage_client = xai_grok_shell::auth::credential_provider::build_storage_client_for_proxy(
        &agent_config.endpoints.proxy_url(),
        deployment_key,
        agent_config.endpoints.alpha_test_key.clone(),
        Some(auth_manager),
        None,
        None,
        "grok-pager",
    );
    let progress: xai_grok_shell::session::restore::ProgressCallback =
        Box::new(|event| eprintln!("  {}", event.display_line()));
    let result = restore_session_with_storage(
        &registry_client,
        &storage_client,
        session_id,
        cwd,
        None,
        Some(progress),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to restore session from remote: {:#}", e))?;
    let effective_id = if result.local_session_id.is_empty() {
        session_id.to_string()
    } else {
        result.local_session_id
    };
    eprintln!("  Restored as local session {}", effective_id);
    Ok(ResolvedExisting {
        id: effective_id,
        original_cwd: None,
        title: None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    fn parse(args: &[&str]) -> PagerArgs {
        PagerArgs::try_parse_from(args).unwrap()
    }
    #[test]
    fn deferred_startup_owner_take_is_atomic() {
        let mut actions = DeferredStartupActions {
            session: Some(DeferredSessionStartup::ForeignResume {
                tool: xai_grok_workspace::foreign_sessions::ForeignSessionTool::Cursor,
                native_id: "cursor-id".into(),
            }),
            prompt: Some("prompt".into()),
            pending_chat: true,
            ..Default::default()
        };
        assert!(!actions.is_empty());
        let snapshot = actions.take();
        assert!(actions.is_empty());
        assert!(snapshot.session.is_some());
        assert_eq!(snapshot.prompt.as_deref(), Some("prompt"));
        assert!(snapshot.pending_chat);
    }
    #[test]
    fn chat_mode_refuses_only_local_build_non_conversation() {
        assert!(chat_mode_refuses_local_build(true, false, true));
        assert!(!chat_mode_refuses_local_build(true, false, false));
        assert!(!chat_mode_refuses_local_build(true, true, true));
        assert!(!chat_mode_refuses_local_build(false, false, true));
    }
    #[test]
    fn intent_default_is_new_auto() {
        assert_eq!(
            parse(&["grok"]).session_startup_intent().unwrap(),
            SessionStartupIntent::NewAuto
        );
    }
    #[test]
    fn intent_resume_id() {
        assert_eq!(
            parse(&["grok", "--resume", "abc"])
                .session_startup_intent()
                .unwrap(),
            SessionStartupIntent::Resume {
                session_id: Some("abc".into()),
                most_recent_for_cwd: false,
            }
        );
    }
    #[test]
    fn intent_resume_empty_is_most_recent() {
        assert_eq!(
            parse(&["grok", "--resume"])
                .session_startup_intent()
                .unwrap(),
            SessionStartupIntent::Resume {
                session_id: None,
                most_recent_for_cwd: true,
            }
        );
    }
    #[test]
    fn intent_continue() {
        assert_eq!(
            parse(&["grok", "-c"]).session_startup_intent().unwrap(),
            SessionStartupIntent::Resume {
                session_id: None,
                most_recent_for_cwd: true,
            }
        );
    }
    #[test]
    fn intent_session_id_alone_is_new_with_id() {
        assert_eq!(
            parse(&["grok", "--session-id", "my-id"])
                .session_startup_intent()
                .unwrap(),
            SessionStartupIntent::NewWithId {
                session_id: "my-id".into(),
            }
        );
    }
    #[test]
    fn intent_session_id_with_resume_without_fork_errors() {
        let err = parse(&["grok", "-r", "a", "-s", "b"])
            .session_startup_intent()
            .unwrap_err();
        assert_eq!(err, StartupFlagError::SessionIdRequiresFork);
    }
    #[test]
    fn intent_fork_with_resume() {
        assert_eq!(
            parse(&["grok", "-r", "old", "--fork-session"])
                .session_startup_intent()
                .unwrap(),
            SessionStartupIntent::ForkFrom {
                source_session_id: Some("old".into()),
                most_recent_for_cwd: false,
                new_session_id: None,
            }
        );
    }
    #[test]
    fn intent_fork_with_resume_and_new_id() {
        assert_eq!(
            parse(&["grok", "-r", "old", "--fork-session", "-s", "new"])
                .session_startup_intent()
                .unwrap(),
            SessionStartupIntent::ForkFrom {
                source_session_id: Some("old".into()),
                most_recent_for_cwd: false,
                new_session_id: Some("new".into()),
            }
        );
    }
    #[test]
    fn intent_fork_alone_errors() {
        let err = parse(&["grok", "--fork-session"])
            .session_startup_intent()
            .unwrap_err();
        assert_eq!(err, StartupFlagError::ForkRequiresResumeOrContinue);
    }
    #[test]
    fn intent_fork_with_worktree_errors() {
        let err = parse(&["grok", "-r", "a", "--fork-session", "-w"])
            .session_startup_intent()
            .unwrap_err();
        assert_eq!(err, StartupFlagError::ForkWithWorktree);
    }
    #[test]
    fn intent_from_flags_matches_pager_args() {
        let args = parse(&["grok", "-r", "old", "--fork-session", "-s", "new"]);
        let from_flags = session_startup_intent_from_flags(SessionStartupFlags {
            session_id: Some("new"),
            resume_session_id: Some("old"),
            resume_most_recent: false,
            continue_last_session: false,
            fork_session: true,
            has_worktree: false,
        })
        .unwrap();
        assert_eq!(from_flags, args.session_startup_intent().unwrap());
    }
    #[test]
    fn ensure_rejects_non_uuid() {
        let err = ensure_session_id_available("my-run-1", "/tmp/does-not-matter").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must be a valid UUID"),
            "unexpected message: {msg}"
        );
    }
    #[test]
    fn effective_fork_new_cwd_prefers_parent() {
        let parent = PathBuf::from("/proj-a");
        assert_eq!(
            effective_fork_new_cwd("/proj-b", Some(parent.as_path())),
            "/proj-a"
        );
        assert_eq!(effective_fork_new_cwd("/proj-b", None), "/proj-b");
    }
    #[test]
    fn fork_session_params_sets_new_session_id_and_workspace_dir() {
        let cwd = PathBuf::from("/wt");
        let p = fork_session_params("parent-1", &cwd, Some("child-uuid"), true);
        assert_eq!(p["sourceSessionId"], "parent-1");
        assert_eq!(p["newCwd"], "/wt");
        assert_eq!(p["newSessionId"], "child-uuid");
        assert_eq!(p["sourceWorkspaceDir"], "/wt");
        assert_eq!(p["sessionKind"], "fork");
    }
    #[test]
    fn fork_session_params_omits_workspace_dir_when_not_worktree() {
        let cwd = PathBuf::from("/proj");
        let p = fork_session_params("parent-1", &cwd, None, false);
        assert!(p.get("sourceWorkspaceDir").is_none());
        assert!(p.get("newSessionId").is_none());
    }
    #[test]
    fn fork_response_parses_nested_and_top_level_id() {
        assert_eq!(
            fork_response_new_session_id(r#"{"newSessionId":"a"}"#).as_deref(),
            Some("a")
        );
        assert_eq!(
            fork_response_new_session_id(r#"{"result":{"newSessionId":"b"}}"#).as_deref(),
            Some("b")
        );
        assert!(fork_response_new_session_id(r#"{"error":"nope"}"#).is_none());
        assert_eq!(
            fork_response_error(r#"{"error":"boom"}"#).as_deref(),
            Some("\"boom\"")
        );
    }
    #[test]
    fn deferred_session_intent_variants_are_distinct() {
        let load = DeferredSessionStartup::Load {
            session_id: "s".into(),
            session_cwd: None,
            chat_kind: false,
        };
        let nid = DeferredSessionStartup::NewWithId {
            session_id: "s".into(),
        };
        assert_ne!(load, nid);
    }
    fn chat_ctx() -> MaterializeCtx {
        MaterializeCtx {
            has_worktree: false,
            allow_remote_restore: true,
            chat_mode: true,
        }
    }
    #[test]
    fn chat_mode_flag_conflict_matrix() {
        assert_eq!(
            chat_mode_flag_conflict(true, true, false),
            Some(CHAT_MODE_FORK_CONFLICT)
        );
        assert_eq!(
            chat_mode_flag_conflict(true, false, true),
            Some(CHAT_MODE_RESTORE_CODE_CONFLICT)
        );
        assert_eq!(
            chat_mode_flag_conflict(true, true, true),
            Some(CHAT_MODE_FORK_CONFLICT)
        );
        assert_eq!(chat_mode_flag_conflict(true, false, false), None);
        assert_eq!(chat_mode_flag_conflict(false, true, true), None);
    }
    #[test]
    fn materialize_ctx_chat_mode_from_args() {
        assert!(!MaterializeCtx::from_pager_args(&parse(&["grok"])).chat_mode);
    }
    /// hardcoded `false` here once disabled it everywhere.
    #[test]
    fn remote_restore_follows_compiled_restore_stack() {
        assert_eq!(
            MaterializeCtx::from_pager_args(&parse(&["grok"])).allow_remote_restore,
            false
        );
    }
    /// Explicit-id resume under `--chat` passes the id through untouched:
    /// no disk resolution, no GCS restore (the cwd does not even exist).
    #[tokio::test]
    async fn materialize_chat_resume_id_is_conversation_direct() {
        let out = materialize_startup_for_cwd(
            chat_ctx(),
            SessionStartupIntent::Resume {
                session_id: Some("conv-e2f1".into()),
                most_recent_for_cwd: false,
            },
            "/nonexistent/cwd/for/chat-resume-test",
        )
        .await
        .unwrap();
        match out {
            MaterializedStartup::Resume {
                session_id,
                original_cwd,
                title,
            } => {
                assert_eq!(session_id, "conv-e2f1");
                assert!(original_cwd.is_none());
                assert!(title.is_none());
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }
    /// Chat-mode passthrough rejects ids that could escape the sessions tree
    /// via the collision check's path join (or are junk for the gateway).
    #[tokio::test]
    async fn materialize_chat_resume_id_rejects_unsafe_shapes() {
        for bad in ["../../../etc/passwd", "a/b", "conv id", "conv\u{7}", ""] {
            let err = materialize_startup_for_cwd(
                chat_ctx(),
                SessionStartupIntent::Resume {
                    session_id: Some(bad.into()),
                    most_recent_for_cwd: false,
                },
                "/tmp",
            )
            .await
            .unwrap_err();
            assert!(
                err.to_string().contains("invalid conversation id"),
                "expected shape rejection for {bad:?}, got: {err}"
            );
        }
        assert!(valid_conversation_id_shape(
            "aaaaaaaa-1111-2222-3333-444444444444"
        ));
        assert!(valid_conversation_id_shape("conv_abc123"));
    }
    /// A no-feature build asked for chat most-recent must fail loudly, not
    /// silently resolve a local Build session.
    #[tokio::test]
    async fn materialize_chat_most_recent_without_feature_bails() {
        let err = materialize_startup_for_cwd(
            chat_ctx(),
            SessionStartupIntent::Resume {
                session_id: None,
                most_recent_for_cwd: true,
            },
            "/nonexistent/cwd/for/no-feature-chat-test",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("chat"), "unexpected error: {err}");
    }
    /// Without `--chat` an unknown id still goes through disk/GCS resolution
    /// (pinned via `allow_remote_restore: false` → strict "does not exist").
    #[tokio::test]
    async fn materialize_resume_id_without_chat_still_resolves_on_disk() {
        let ctx = MaterializeCtx {
            has_worktree: false,
            allow_remote_restore: false,
            chat_mode: false,
        };
        let err = materialize_startup_for_cwd(
            ctx,
            SessionStartupIntent::Resume {
                session_id: Some("00000000-dead-beef-0000-000000000000".into()),
                most_recent_for_cwd: false,
            },
            "/nonexistent/cwd/for/build-resume-test",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "unexpected error: {err}"
        );
    }
    #[tokio::test]
    async fn materialize_fork_refused_under_chat_mode() {
        for intent in [
            SessionStartupIntent::ForkFrom {
                source_session_id: Some("conv-1".into()),
                most_recent_for_cwd: false,
                new_session_id: None,
            },
            SessionStartupIntent::ForkFrom {
                source_session_id: None,
                most_recent_for_cwd: true,
                new_session_id: None,
            },
        ] {
            let err = materialize_startup_for_cwd(chat_ctx(), intent, "/tmp")
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), CHAT_MODE_FORK_CONFLICT);
        }
    }
    /// The chat passthrough does not bypass the cwd-collision refusal that
    /// `app/mod.rs` runs on the materialized id.
    #[serial_test::serial(GROK_HOME)]
    #[tokio::test]
    async fn chat_resume_passthrough_keeps_cwd_collision_refusal() {
        let home = tempfile::tempdir().expect("home tempdir");
        unsafe { std::env::set_var("GROK_HOME", home.path()) };
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let id = "aaaaaaaa-1111-2222-3333-444444444444";
        let encoded = xai_grok_shell::util::grok_home::encode_cwd_dirname(&cwd_str);
        let sessions_cwd_dir = xai_grok_shell::util::grok_home::grok_home()
            .join("sessions")
            .join(&encoded);
        struct RmDirOnDrop(std::path::PathBuf);
        impl Drop for RmDirOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = RmDirOnDrop(sessions_cwd_dir.clone());
        let session_dir = sessions_cwd_dir.join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let out = materialize_startup_for_cwd(
            chat_ctx(),
            SessionStartupIntent::Resume {
                session_id: Some(id.into()),
                most_recent_for_cwd: false,
            },
            &cwd_str,
        )
        .await
        .unwrap();
        match &out {
            MaterializedStartup::Resume { session_id, .. } => {
                assert_eq!(session_id, id);
                assert!(
                    chat_mode_refuses_local_build_load(true, false, session_id, cwd.path()),
                    "cwd-local Build collision must still be refused after passthrough"
                );
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }
}
