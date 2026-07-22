#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use agent_client_protocol as acp;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::session::{
    self, SessionCommand, SessionHandle, SessionThread,
    commands::{PromptCompletionKind, PromptTurnResult as SubagentPromptTurnResult},
    fs_watch::FsWatchCapabilities, info::Info as SessionInfo,
};
use crate::terminal::AsyncTerminalRunner;
use crate::tools::ToolContext;
use crate::upload::trace::{
    GCS_SCHEMA_VERSION, PromptMetadata, SubagentSpawnedRef, TurnResultMetadata,
    local_sandbox_telemetry, upload_metadata, upload_session_state,
    upload_subagent_metadata, upload_turn_result,
};
use crate::upload::turn::{PromptTraceContext, complete_prompt_trace};
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_tools::implementations::grok_build::task::types::*;
use xai_grok_workspace::file_system::AsyncFileSystem;
use xai_hunk_tracker::HunkTrackerHandle;
use super::*;
/// Remove the task tool (and orphaned background-task actions) from a child
/// toolset at or beyond `MAX_SUBAGENT_DEPTH`. Returns whether the task tool
/// was removed.
pub(super) fn strip_task_tools_at_max_depth(
    tool_config: &mut xai_grok_tools::registry::types::ToolServerConfig,
    child_depth: u32,
) -> bool {
    use xai_grok_tools::implementations::grok_build::task::MAX_SUBAGENT_DEPTH;
    use xai_grok_tools::types::tool::ToolKind;
    if child_depth < MAX_SUBAGENT_DEPTH {
        return false;
    }
    let before = tool_config.tools.len();
    tool_config.tools.retain(|tc| tc.kind != Some(ToolKind::Task));
    let stripped = tool_config.tools.len() < before;
    prune_orphaned_background_task_tools(tool_config);
    stripped
}
pub(super) fn canonical_total_tokens(totals: &xai_chat_state::UsageTotals) -> u64 {
    totals.total_tokens()
}
pub(super) fn usage_is_incomplete(
    ledger_incomplete: bool,
    cancellation_may_hide_usage: bool,
    _known_total_tokens: u64,
    _has_usage_entries: bool,
) -> bool {
    ledger_incomplete || cancellation_may_hide_usage
}
pub(super) fn task_model_override_error(
    requested: Option<&str>,
    provenance: ModelOverrideProvenance,
    is_resume: bool,
    available: &indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    if provenance != ModelOverrideProvenance::Tool || is_resume {
        return None;
    }
    let requested = requested?;
    crate::agent::models::task_model_error_for_catalog(
        requested,
        available,
        is_session_auth,
    )
}
/// This is a free async function, NOT a method on MvpAgent. It receives
/// a `SubagentSpawnContext` with everything it needs, and a mutable
/// reference to the coordinator for tracking.
///
/// Returns when the child session completes (or fails/cancels).
#[tracing::instrument(
    name = "subagent.handle_request",
    skip_all,
    fields(
        subagent_id = %request.id,
        parent_session_id = %ctx.parent_session_id,
        subagent_type = %request.subagent_type,
    )
)]
pub(crate) async fn handle_subagent_request(
    mut request: SubagentRequest,
    mut ctx: SubagentSpawnContext,
    coordinator: &std::cell::RefCell<SubagentCoordinator>,
    gateway: &GatewaySender,
) {
    let start = std::time::Instant::now();
    let mut parent_wait_guard = subagent_blocks_parent_turn(&request)
        .then(|| crate::tools::tool_context::BlockingWaitGuard::enter(
            ctx.parent_blocking_wait_depth.clone(),
        ));
    if request.owner.is_workflow() && request.cancel_token.is_cancelled() {
        parent_wait_guard.take();
        send_pre_spawn_cancelled(request, "Subagent was cancelled");
        return;
    }
    let Some(mut definition) = resolve_agent_definition(&request.subagent_type, &ctx)
    else {
        let msg = format!("Unknown subagent type: {}", request.subagent_type);
        send_pre_spawn_failure(request, &msg, coordinator, &ctx, gateway);
        return;
    };
    match gate_subagent_type(&request.subagent_type, &ctx) {
        SubagentValidateTypeOutcome::Disabled => {
            let msg = format!(
                "Subagent '{}' is disabled via [subagents.toggle] in config.toml",
                request.subagent_type
            );
            send_pre_spawn_failure(request, &msg, coordinator, &ctx, gateway);
            return;
        }
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            let msg = format!(
                "agent can only spawn: {}; '{}' not allowed", allowed.join(", "), request
                .subagent_type
            );
            send_pre_spawn_failure(request, &msg, coordinator, &ctx, gateway);
            return;
        }
        _ => {}
    }
    let run_in_background = request.run_in_background
        || definition.background.unwrap_or(false);
    let cancel_token = request.cancel_token.clone();
    coordinator
        .borrow_mut()
        .insert_pending(PendingSubagent {
            subagent_id: request.id.clone(),
            subagent_type: request.subagent_type.clone(),
            description: request.description.clone(),
            persona: request.runtime_overrides.persona.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
            parent_session_id: ctx.parent_session_id.clone(),
            owner: request.owner.clone(),
            started_at: start,
            run_in_background,
            surface_completion: request.surface_completion,
            color: definition.color,
            cancel_token: cancel_token.clone(),
        });
    let mut pending_guard = PendingGuard {
        coordinator,
        id: request.id.clone(),
        defused: false,
        error: None,
    };
    resolve_subagent_toolset(
        &request.subagent_type,
        request.runtime_overrides.harness_agent_type.as_deref(),
        &ctx,
        &mut definition,
    );
    let (role, role_key) = {
        let by_type = ctx.subagent_roles.get(&request.subagent_type);
        if by_type.is_some() {
            (by_type, Some(request.subagent_type.clone()))
        } else {
            let by_persona = request
                .runtime_overrides
                .persona
                .as_deref()
                .and_then(|p| ctx.subagent_roles.get(p));
            let key = if by_persona.is_some() {
                request.runtime_overrides.persona.clone()
            } else {
                None
            };
            (by_persona, key)
        }
    };
    let cwd = ctx.parent_session_info.as_ref().map(|i| std::path::Path::new(&i.cwd));
    let effective_runtime = resolve_effective_overrides(
        &request.runtime_overrides,
        role,
        &ctx.subagent_personas,
        cwd,
        role_key,
    );
    let mut effective_runtime = effective_runtime;
    if effective_runtime.reasoning_effort.is_none() {
        effective_runtime.reasoning_effort = definition
            .effort
            .map(|e| <&str>::from(e).to_string());
    }
    {
        use xai_tool_types::SubagentIsolationMode;
        if effective_runtime.isolation == SubagentIsolationMode::None
            && definition.isolation
                == Some(xai_grok_agent::config::IsolationMode::Worktree)
        {
            effective_runtime.isolation = SubagentIsolationMode::Worktree;
        }
    }
    let prompt = request.prompt.clone();
    if let Some(ref err) = effective_runtime.persona_error {
        tracing::error!(
            subagent_id = % request.id, error = err,
            "Persona resolution failed, aborting subagent spawn"
        );
        pending_guard.set_error(err.clone());
        send_failure(request, err);
        return;
    }
    if let Some(ref warn) = effective_runtime.role_prompt_warning {
        tracing::warn!(
            subagent_id = % request.id, warning = warn,
            "Role prompt_file degraded, continuing without role prompt"
        );
    }
    let resume_source = if let Some(resume_id) = request
        .resume_from
        .as_deref()
        .filter(|s| is_valid_resume_id(s))
    {
        let coord = coordinator.borrow();
        if coord.is_active(resume_id) {
            let msg = format!(
                "Cannot resume from subagent '{resume_id}': it is still running. \
                 Wait for it to complete before resuming."
            );
            drop(coord);
            send_failure(request, &msg);
            return;
        }
        match coord
            .resumable_source_for(resume_id, &ctx.parent_session_id, &ctx.parent_cwd)
        {
            Some(info) => {
                drop(coord);
                Some(info)
            }
            None => {
                let msg = format!(
                    "Cannot resume from subagent '{resume_id}': not found. \
                     The subagent may have been evicted or the ID is invalid."
                );
                drop(coord);
                send_failure(request, &msg);
                return;
            }
        }
    } else {
        None
    };
    if let Some(ref source) = resume_source {
        if request.runtime_overrides.model.is_some() {
            tracing::debug!(
                subagent_id = % request.id,
                "Ignoring caller model override on resume; source model will be pinned"
            );
        }
        effective_runtime.model = None;
        if let Err(e) = xai_grok_subagent_resolution::validate_resume_identity(
            &request.subagent_type,
            request.runtime_overrides.persona.as_deref(),
            source,
        ) {
            send_failure(request, &e.to_string());
            return;
        }
    }
    if let Some(error) = task_model_override_error(
        request.runtime_overrides.model.as_deref(),
        request.runtime_overrides.model_override_provenance,
        resume_source.is_some(),
        &ctx.available_models,
        ctx.auth_manager.current_or_expired().is_some_and(|a| a.is_session_auth()),
    ) {
        pending_guard.set_error(error.clone());
        send_failure(request, &error);
        return;
    }
    let worktree_path = if let Some(ref source) = resume_source {
        if effective_runtime.isolation != xai_tool_types::SubagentIsolationMode::None
            && source.worktree_path.is_none()
        {
            tracing::info!(
                subagent_id = % request.id,
                "Ignoring isolation=worktree override: resumed source had no worktree"
            );
        }
        match source.worktree_path.as_deref() {
            None => None,
            Some(dest) => {
                match resume_worktree_action(
                    dest.is_dir(),
                    source.snapshot_ref.as_deref(),
                ) {
                    ResumeWorktreeAction::Reuse => Some(dest.to_path_buf()),
                    ResumeWorktreeAction::Rehydrate => {
                        let snapshot_ref = source
                            .snapshot_ref
                            .clone()
                            .unwrap_or_default();
                        let source_repo = resolve_subagent_source_repo(&ctx);
                        match crate::session::worktree::rehydrate_subagent_worktree(
                                dest,
                                &source_repo,
                                &snapshot_ref,
                                Some(source.subagent_id.as_str()),
                            )
                            .await
                        {
                            Ok(path) => {
                                tracing::info!(
                                    subagent_id = % request.id, worktree_path = % path
                                    .display(), snapshot_ref = % snapshot_ref,
                                    "Rehydrated subagent worktree from snapshot for resume"
                                );
                                Some(path)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    subagent_id = % request.id, error = % e,
                                    "Failed to rehydrate subagent worktree, falling back to shared workspace"
                                );
                                None
                            }
                        }
                    }
                    ResumeWorktreeAction::Shared => {
                        tracing::warn!(
                            subagent_id = % request.id, worktree = % dest.display(),
                            "Resumed subagent worktree dir missing with no snapshot; using shared workspace"
                        );
                        None
                    }
                }
            }
        }
    } else if effective_runtime.isolation != xai_tool_types::SubagentIsolationMode::None
    {
        let source_cwd = parent_source_cwd(&ctx);
        let dest = match crate::session::worktree::worktree_base_dir_for_source(
            &source_cwd,
        ) {
            Ok(base) => base.join(format!("subagent-{}", request.id)),
            Err(e) => {
                tracing::warn!(
                    subagent_id = % request.id, error = % e,
                    "Could not resolve worktree base dir, using temp dir for subagent worktree"
                );
                std::env::temp_dir().join("grok-subagent-worktrees").join(&request.id)
            }
        };
        let source_clone = source_cwd;
        let subagent_id = request.id.clone();
        let creation_mode: xai_fast_worktree::CreationMode = ctx.worktree_type.into();
        let btrfs_delegate = crate::session::worktree::btrfs_delegate_from_env();
        match tokio::task::spawn_blocking(move || {
                let mut builder = xai_fast_worktree::WorktreeBuilder::new(
                        &source_clone,
                        &dest,
                    )
                    .working_tree_mode(
                        xai_fast_worktree::WorkingTreeMode::PreserveWorkingTree,
                    )
                    .creation_mode(creation_mode)
                    .worktree_kind(xai_fast_worktree::WorktreeKind::Subagent)
                    .session_id(subagent_id);
                if let Some(delegate) = btrfs_delegate {
                    builder = builder.btrfs_delegate(delegate);
                }
                builder.create()
            })
            .await
        {
            Ok(Ok(report)) => {
                tracing::info!(
                    subagent_id = % request.id, worktree_path = % report.worktree_path
                    .display(), commit = % report.commit,
                    "Created isolated worktree for subagent"
                );
                Some(report.worktree_path)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    subagent_id = % request.id, error = % e,
                    "Failed to create worktree, falling back to shared workspace"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    subagent_id = % request.id, error = % e,
                    "Worktree creation task panicked, falling back to shared workspace"
                );
                None
            }
        }
    } else {
        None
    };
    let worktree_freshly_created = resume_source.is_none() && worktree_path.is_some();
    if let Some(raw_cwd) = request.cwd.as_deref() {
        match sanitize_cwd_value(raw_cwd) {
            Some(cwd_path) => {
                if worktree_path.is_none() && resume_source.is_none() {
                    let p = Path::new(&cwd_path);
                    if !p.is_dir() {
                        let msg = if p.exists() {
                            format!("cwd \"{cwd_path}\" exists but is not a directory")
                        } else {
                            format!("cwd \"{cwd_path}\" does not exist")
                        };
                        send_failure(request, &msg);
                        return;
                    }
                }
                request.cwd = Some(cwd_path);
            }
            None => request.cwd = None,
        }
    }
    if effective_runtime.reasoning_effort.is_some()
        || effective_runtime.capability_mode.is_some()
    {
        tracing::info!(
            subagent_id = % request.id, reasoning_effort = ? effective_runtime
            .reasoning_effort, capability_mode = ? effective_runtime.capability_mode,
            "Resolved runtime overrides for subagent"
        );
    }
    effective_runtime.capability_mode = xai_grok_subagent_resolution::intersect_capability_modes(
        effective_runtime.capability_mode,
        definition.capability_mode,
    );
    if let Some(mode) = effective_runtime.capability_mode {
        mode.filter_tool_config(&mut definition.tool_config);
        tracing::info!(
            subagent_id = % request.id, capability_mode = ? mode, tools_remaining =
            definition.tool_config.tools.len(),
            "Applied capability mode filter to agent tool config"
        );
    }
    let child_depth = request
        .runtime_overrides
        .spawn_depth
        .unwrap_or(ctx.parent_depth + 1);
    if strip_task_tools_at_max_depth(&mut definition.tool_config, child_depth) {
        tracing::info!(
            subagent_id = % request.id, child_depth,
            "Stripped task tool from child at max depth"
        );
    }
    if request.owner.is_workflow() {
        definition
            .tool_config
            .tools
            .retain(|tool| {
                !matches!(
                    tool.id.rsplit(':').next(), Some("scheduler_create" |
                    "scheduler_list" | "scheduler_delete")
                )
            });
    }
    if request.fork_context {
        effective_runtime.model = Some(ctx.model_id.0.to_string());
    }
    let (mut effective_sampling_config, mut effective_model_id) = resolve_effective_model_config(
            effective_runtime.model.as_deref(),
            &request.subagent_type,
            &definition.model,
            &ctx,
        )
        .await;
    let subagent_max_turns = resolve_subagent_max_turns(
        definition.max_turns,
        ctx.parent_max_turns,
    );
    {
        let model_str = &effective_sampling_config.model;
        let model_unknown = !model_str.is_empty() && !ctx.available_models.is_empty()
            && !ctx.available_models.contains_key(model_str)
            && !ctx.available_models.values().any(|e| e.info().model == *model_str);
        if model_unknown {
            let (parent_config, parent_mid) = read_parent_sampling_config(&ctx).await;
            tracing::warn!(
                subagent_id = % request.id, resolved_model = % model_str, parent_model =
                % parent_config.model,
                "Resolved subagent model not found in available models — \
                 falling back to parent model"
            );
            effective_sampling_config = parent_config;
            effective_model_id = parent_mid;
        }
    }
    if let Some(ref source) = resume_source
        && let Some(ref source_model) = source.model_id
        && effective_model_id.0.as_ref() != source_model.as_str()
    {
        if let Some(resolved) = resolve_model_override_to_config(source_model, &ctx) {
            tracing::info!(
                subagent_id = % request.id, resolved_model = % effective_model_id.0,
                source_model = source_model, "Pinning resumed child to source model"
            );
            effective_sampling_config = resolved.0;
            effective_model_id = resolved.1;
        } else {
            let msg = format!(
                "Cannot resume from subagent '{}': source model '{source_model}' \
                 is no longer available in the model catalogue.",
                source.subagent_id,
            );
            send_failure(request, &msg);
            return;
        }
    }
    if let Some(raw) = effective_runtime.reasoning_effort.as_deref()
        && ctx
            .models_manager
            .model_supports_reasoning_effort(effective_model_id.0.as_ref())
    {
        use xai_grok_sampling_types::ReasoningEffort;
        match raw.parse::<ReasoningEffort>() {
            Ok(eff) => effective_sampling_config.reasoning_effort = Some(eff),
            Err(err) => {
                tracing::warn!(
                    value = raw, error = % err,
                    "subagent reasoning_effort: parse failed, ignoring override"
                )
            }
        }
    }
    let subagent_id = request.id.clone();
    let child_session_id = acp::SessionId::new(subagent_id.clone());
    let override_cwd = select_override_cwd(
        resume_source.as_ref(),
        request.cwd.as_deref(),
    );
    let effective_cwd = resolve_child_cwd(
            worktree_path.as_deref(),
            override_cwd,
            &ctx.parent_cwd,
        )
        .to_string_lossy()
        .into_owned();
    let child_session_info = SessionInfo {
        id: child_session_id.clone(),
        cwd: effective_cwd,
    };
    let child_session_dir = session::persistence::session_dir(&child_session_info);
    let parent_session_dir = session::persistence::session_dir(
        &SessionInfo {
            id: acp::SessionId::new(ctx.parent_session_id.clone()),
            cwd: ctx.parent_cwd.to_string_lossy().to_string(),
        },
    );
    let subagent_meta_dir = parent_session_dir.join("subagents").join(&subagent_id);
    let InitialContext {
        source: context_source,
        copy_error: fork_copy_error,
        prefix_len: inherited_prefix_len,
        conversation: forked_conversation,
        verbatim_fork: context_verbatim_fork,
    } = match bootstrap_initial_context(
            &request,
            resume_source.as_ref(),
            &ctx,
            &child_session_info,
            &child_session_dir,
            effective_model_id.0.as_ref(),
            effective_sampling_config.context_window,
        )
        .await
    {
        BootstrapInitialContext::Ready(ctx) => ctx,
        BootstrapInitialContext::ResumeAbort(msg) => {
            tracing::error!(
                subagent_id = % request.id, error = % msg,
                "Resume-copy failed, aborting subagent spawn"
            );
            send_failure(request, &msg);
            return;
        }
    };
    let verbatim_mirror_fork = context_source == InitialContextSource::Forked
        && context_verbatim_fork;
    let task_prompt_text = prompt.clone();
    let (mut forked_conversation, mut inherited_prefix_len) = (
        forked_conversation,
        inherited_prefix_len.unwrap_or(0),
    );
    if context_source != InitialContextSource::Resumed && !verbatim_mirror_fork
        && let Some(ref pi) = effective_runtime.persona_instructions
    {
        let reminder = xai_grok_sampling_types::conversation::ConversationItem::system_reminder(
            format!("<system-reminder>\n{pi}\n</system-reminder>"),
        );
        let insert_at = inherited_prefix_len.min(forked_conversation.len());
        forked_conversation.insert(insert_at, reminder);
        inherited_prefix_len += 1;
    }
    let effective_source_str = match &context_source {
        InitialContextSource::New => "new",
        InitialContextSource::Forked => "forked",
        InitialContextSource::Resumed => "resumed",
    };
    let subagent_meta = SubagentMeta {
        subagent_id: subagent_id.clone(),
        parent_session_id: ctx.parent_session_id.clone(),
        child_session_id: child_session_id.0.to_string(),
        subagent_type: request.subagent_type.clone(),
        description: request.description.clone(),
        prompt: request.prompt.clone(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some(effective_source_str.to_string()),
        context_normalized: fork_context_normalized(
            &context_source,
            context_verbatim_fork,
        ),
        fork_copy_error: fork_copy_error.clone(),
        persona: effective_runtime.persona.clone(),
        resumed_from: request.resume_from.clone(),
        child_cwd: Some(child_session_info.cwd.clone()),
        worktree_path: worktree_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        snapshot_ref: None,
        effective_model_id: Some(effective_model_id.0.to_string()),
    };
    write_subagent_meta(&subagent_meta_dir, &subagent_meta);
    if let (Some(bucket_url), Some(upload_method)) = (
        &ctx.gcs_bucket_url,
        &ctx.gcs_upload_method,
    ) {
        let gcs_meta = SubagentSessionMetadata::from_meta(
            &subagent_meta,
            Some(&*effective_model_id.0),
            Some(&child_session_info.cwd),
            None,
            None,
            None,
            effective_runtime.reasoning_effort.as_deref(),
            effective_runtime.role_name.as_deref(),
            request.parent_prompt_id.as_deref(),
            0,
        );
        let bucket = bucket_url.clone();
        let method = upload_method.clone();
        let auth_for_spawn = ctx.auth_manager.clone();
        tokio::spawn(async move {
            upload_subagent_metadata(&gcs_meta, &bucket, method, auth_for_spawn).await;
        });
    }
    let gcs_upload_ctx = GcsUploadContext {
        bucket_url: ctx.gcs_bucket_url.clone(),
        upload_method: ctx.gcs_upload_method.clone(),
        model_id: Some(effective_model_id.0.to_string()),
        cwd: Some(child_session_info.cwd.clone()),
        reasoning_effort: effective_runtime.reasoning_effort.clone(),
        role_name: effective_runtime.role_name.clone(),
        parent_prompt_id: request.parent_prompt_id.clone(),
        auth_manager: ctx.auth_manager.clone(),
        isolation_mode: Some(format!("{:?}", effective_runtime.isolation)),
        capability_mode: effective_runtime
            .capability_mode
            .as_ref()
            .map(|m| format!("{m:?}")),
        depth: child_depth,
    };
    emit_subagent_notification(
        gateway,
        &ctx.parent_session_id,
        SessionUpdate::SubagentSpawned {
            subagent_id: subagent_id.clone(),
            child_session_id: child_session_id.0.to_string(),
            parent_session_id: ctx.parent_session_id.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
            subagent_type: request.subagent_type.clone(),
            description: request.description.clone(),
            effective_context_source: Some(effective_source_str.to_string()),
            context_normalized: fork_context_normalized(
                &context_source,
                context_verbatim_fork,
            ),
            capability_mode: effective_runtime
                .capability_mode
                .and_then(|m| serde_json::to_value(m).ok())
                .and_then(|v| v.as_str().map(String::from)),
            persona: effective_runtime.persona.clone(),
            role: effective_runtime.role_name.clone(),
            model: Some(effective_model_id.0.to_string()),
            resumed_from: request.resume_from.clone(),
            workflow_run_id: request.owner.workflow_run_id().map(str::to_string),
        },
        ctx.parent_cmd_tx.as_ref(),
    );
    let early_gcs_ctx = GcsUploadContext {
        bucket_url: ctx.gcs_bucket_url.clone(),
        upload_method: ctx.gcs_upload_method.clone(),
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: effective_runtime.reasoning_effort.clone(),
        role_name: effective_runtime.role_name.clone(),
        parent_prompt_id: request.parent_prompt_id.clone(),
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    };
    let sampling_client = match crate::sampling::Client::new(
        effective_sampling_config.clone(),
    ) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Sampling client error: {e}");
            pending_guard.set_error(msg.clone());
            fail_subagent(
                request,
                &msg,
                &subagent_id,
                &child_session_id,
                &subagent_meta_dir,
                gateway,
                &ctx.parent_session_id,
                ctx.parent_cmd_tx.as_ref(),
                0,
                &early_gcs_ctx,
            );
            return;
        }
    };
    let persistence = match session::persistence::new_with_explicit_dir(
            &child_session_info,
            child_session_dir.clone(),
            effective_model_id.clone(),
            sampling_client,
            effective_sampling_config.model.clone(),
        )
        .await
    {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("Persistence error: {e}");
            pending_guard.set_error(msg.clone());
            fail_subagent(
                request,
                &msg,
                &subagent_id,
                &child_session_id,
                &subagent_meta_dir,
                gateway,
                &ctx.parent_session_id,
                ctx.parent_cmd_tx.as_ref(),
                0,
                &early_gcs_ctx,
            );
            return;
        }
    };
    let child_cwd = resolve_child_cwd(
        worktree_path.as_deref(),
        override_cwd,
        &ctx.parent_cwd,
    );
    let cwd_outside_parent = match (
        dunce::canonicalize(&child_cwd),
        dunce::canonicalize(&ctx.parent_cwd),
    ) {
        (Ok(child), Ok(parent)) => !child.starts_with(&parent),
        _ => child_cwd != ctx.parent_cwd,
    };
    let subagent_fs_watch = FsWatchCapabilities {
        hunk_tracking: ctx.hunk_tracking_enabled && cwd_outside_parent,
        ..FsWatchCapabilities::none()
    };
    let child_cwd_abs = xai_grok_paths::AbsPathBuf::new(child_cwd)
        .unwrap_or_else(|_| {
            xai_grok_paths::AbsPathBuf::new(std::env::current_dir().unwrap_or_default())
                .expect("current_dir should be absolute")
        });
    let mut tool_ctx = ToolContext::with_preloaded_env(
            child_cwd_abs,
            Some(gateway.clone()),
            Some(child_session_id.clone()),
            ctx.fs.clone(),
            ctx.terminal.clone(),
            ctx.hunk_tracker_handle.clone(),
            (*ctx.session_env).clone(),
        )
        .with_hunk_tracking_enabled(ctx.hunk_tracking_enabled);
    tool_ctx.subagent_event_tx = Some(ctx.subagent_event_tx.clone());
    let task_output_budget = request
        .runtime_overrides
        .output_token_budget
        .map(crate::tools::tool_context::TaskOutputTokenBudget::limited);
    tool_ctx.task_output_token_budget = task_output_budget.clone();
    tool_ctx.sampler_retry_only_before_output = task_output_budget.is_some();
    tool_ctx.monitor_event_buffer = Some(MonitorEventBuffer::default());
    tool_ctx.subagent_depth = child_depth;
    tool_ctx.lsp = ctx.lsp.clone();
    let parent_traceparent = xai_file_utils::trace_context::current_traceparent();
    let tracker_child_cwd = child_session_info.cwd.clone();
    let tracker_model_id = effective_model_id.0.to_string();
    let initial_child_tokens = xai_chat_state::estimate_conversation_tokens(
        &forked_conversation,
    );
    let model_entry = crate::agent::config::find_model_by_id(
        &ctx.available_models,
        effective_model_id.0.as_ref(),
    );
    let model_has_own_creds = model_entry
        .is_some_and(|entry| entry.has_own_credentials());
    let inherited_auth_type = subagent_auth_type(model_entry, &ctx.auth_method_id);
    let credentials = xai_chat_state::Credentials {
        api_key: effective_sampling_config.api_key.clone(),
        auth_type: inherited_auth_type,
        alpha_test_key: ctx.alpha_test_key.clone(),
        client_version: effective_sampling_config.client_version.clone(),
    };
    xai_grok_telemetry::unified_log::info(
        "subagent spawn credentials",
        None,
        Some(
            serde_json::json!(
                { "subagent_id" : & request.id, "subagent_type" : & request
                .subagent_type, "effective_model" : effective_model_id.0.as_ref(),
                "effective_model_raw" : & effective_sampling_config.model, "base_url" : &
                effective_sampling_config.base_url, "key_prefix" : key_prefix(&
                effective_sampling_config.api_key), "auth_type" : format!("{:?}",
                inherited_auth_type), "model_has_own_creds" : model_has_own_creds,
                "auth_method_id" : ctx.auth_method_id.0.as_ref(), "parent_model" : ctx
                .model_id.0.as_ref(), "parent_key_prefix" : key_prefix(& ctx
                .sampling_config.api_key), "context_window" : effective_sampling_config
                .context_window, }
            ),
        ),
    );
    let attribution_callback: Option<xai_grok_sampler::SharedAttributionCallback> = effective_sampling_config
        .attribution_callback
        .clone();
    let tracker_color = definition.color;
    let agent_memory_scope = definition.memory;
    let agent_name_for_memory = definition.name.clone();
    let is_plugin_agent = definition.plugin_name.is_some();
    let yolo_policy_block = xai_grok_workspace::permission::resolution::yolo_disabled_by_policy();
    let agent_permission_mode = resolve_subagent_permission_mode(
        definition.permission_mode.clone(),
        is_plugin_agent,
        yolo_policy_block,
    );
    if agent_permission_mode != definition.permission_mode {
        if is_plugin_agent {
            tracing::warn!(
                agent = % definition.name, plugin = ? definition.plugin_name,
                "ignoring permissionMode on plugin agent (not supported for security)"
            );
        } else {
            tracing::warn!(
                agent = % definition.name,
                "ignoring subagent permissionMode=bypassPermissions: always-approve disabled by managed policy"
            );
        }
    }
    if let Some(scope) = agent_memory_scope {
        use xai_grok_tools::implementations::grok_build;
        use xai_grok_tools::implementations::opencode;
        let memory_tools: Vec<xai_grok_tools::registry::types::ToolConfig> = vec![
            (& grok_build::ReadFileTool).into(), (& grok_build::SearchReplaceTool)
            .into(), (& opencode::OpenCodeWriteTool).into(),
        ];
        for tc in memory_tools {
            if !definition.tool_config.tools.iter().any(|t| t.id == tc.id) {
                definition.tool_config.tools.push(tc);
            }
        }
        let resolved_mem = scope.resolve_dir(&agent_name_for_memory, &ctx.parent_cwd);
        let memory_dir = &resolved_mem.path;
        let memory_md = memory_dir.join("MEMORY.md");
        if memory_md.is_file() && let Ok(content) = std::fs::read_to_string(&memory_md) {
            const MAX_LINES: usize = 200;
            const MAX_BYTES: usize = 25 * 1024;
            let truncated: String = content
                .lines()
                .take(MAX_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            let truncated = xai_grok_tools::util::truncate::truncate_str(
                    &truncated,
                    MAX_BYTES,
                )
                .to_string();
            if !truncated.is_empty() {
                let injection = format!(
                    "\n\n<agent-memory>\nMemory directory: {}\n\n{truncated}\n</agent-memory>",
                    memory_dir.display()
                );
                definition.prompt_body = Some(
                    definition.prompt_body.unwrap_or_default() + injection.as_str(),
                );
            }
        }
    }
    let is_plugin_agent = definition.plugin_name.is_some();
    if let Some(ref hooks_config) = definition.hooks {
        if is_plugin_agent {
            tracing::warn!(
                agent = % definition.name, plugin = ? definition.plugin_name,
                "ignoring hooks on plugin agent (not supported for security)"
            );
        } else if !crate::agent::folder_trust::agent_inline_hooks_allowed(
            definition.scope,
            || crate::agent::folder_trust::project_scope_allowed(&ctx.parent_cwd),
        ) {
            tracing::warn!(
                agent = % definition.name,
                "ignoring hooks on untrusted project agent (folder not trusted; re-run with --trust)"
            );
        } else {
            let hooks_val = hooks_config.as_value();
            let (specs, errors) = xai_grok_hooks::config::parse_hooks_from_value_with_dir(
                &hooks_val,
                &format!("agent:{}", definition.name),
                &ctx.parent_cwd,
            );
            for e in &errors {
                tracing::warn!(
                    agent = % definition.name, error = ? e, "agent hook parse error"
                );
            }
            if !specs.is_empty() {
                let specs: Vec<_> = specs
                    .into_iter()
                    .map(|mut s| {
                        if s.event == xai_grok_hooks::event::HookEventName::Stop {
                            s.event = xai_grok_hooks::event::HookEventName::SubagentStop;
                        }
                        s
                    })
                    .collect();
                let mut registry = ctx
                    .hook_registry
                    .as_ref()
                    .map(|r| (**r).clone())
                    .unwrap_or_default();
                registry.append_specs(specs);
                ctx.hook_registry = Some(std::sync::Arc::new(registry));
            }
        }
    }
    let agent_mcp_servers: Vec<_> = if is_plugin_agent {
        if !definition.mcp_servers.is_empty() {
            tracing::warn!(
                agent = % definition.name, plugin = ? definition.plugin_name,
                "ignoring mcpServers on plugin agent (not supported for security)"
            );
        }
        vec![]
    } else {
        definition
                .mcp_servers
                .iter()
                .filter_map(|entry| match entry {
                    xai_grok_agent::config::McpServerRef::Named(name) => {
                        ctx.parent_mcp_configs
                            .iter()
                            .find(|s| {
                                crate::session::mcp_servers::mcp_server_name(s) == name
                            })
                            .cloned()
                            .or_else(|| {
                                tracing::warn!(
                                    agent = % definition.name, server = name,
                                    "mcpServers: named ref not found in parent"
                                );
                                None
                            })
                    }
                    xai_grok_agent::config::McpServerRef::Inline { name, config } => {
                        if let serde_json::Value::Object(obj) = config
                            && obj.contains_key("type")
                        {
                            let mut flat = obj.clone();
                            flat.insert(
                                "name".to_string(),
                                serde_json::Value::String(name.clone()),
                            );
                            if let Ok(server) = serde_json::from_value::<
                                agent_client_protocol::McpServer,
                            >(serde_json::Value::Object(flat)) {
                                return Some(server);
                            }
                            tracing::debug!(
                                agent = % definition.name, server = name,
                                "ACP wire format parse failed, trying map-keyed"
                            );
                        }
                        if let Some(inner_obj) = config.as_object() {
                            let mut flat = inner_obj.clone();
                            flat.insert(
                                "name".to_string(),
                                serde_json::Value::String(name.clone()),
                            );
                            if let Ok(server) = serde_json::from_value::<
                                agent_client_protocol::McpServer,
                            >(serde_json::Value::Object(flat)) {
                                return Some(server);
                            }
                        }
                        tracing::warn!(
                            agent = % definition.name, server = name,
                            "mcpServers: inline config could not be parsed"
                        );
                        None
                    }
                })
                .collect()
    };
    let parent_mcp_pool = if is_plugin_agent {
        if ctx.parent_mcp_pool.is_some() {
            tracing::debug!(
                agent = % definition.name,
                "skipping MCP pool inheritance for plugin agent"
            );
        }
        None
    } else {
        ctx.parent_mcp_pool
                .take()
                .and_then(|pool| filter_pool_by_inheritance(
                    pool,
                    &definition.mcp_inheritance,
                ))
    };
    let mcp_inherited_count = parent_mcp_pool
        .as_ref()
        .map(|p| p.len() as u32)
        .unwrap_or(0);
    if mcp_inherited_count > 0 {
        tracing::info!(
            subagent_id = % request.id, mcp_count = mcp_inherited_count,
            "Subagent inherited MCP servers from parent pool"
        );
    }
    let inherit_skills = definition.inherit_skills;
    if inherit_skills && ctx.parent_skills.is_none() {
        let parent_cwd_str = ctx.parent_cwd.to_string_lossy().to_string();
        ctx.parent_skills = Some(
            xai_grok_agent::prompt::skills::list_skills_with_plugins(
                    Some(&parent_cwd_str),
                    &ctx.parent_skills_config,
                    ctx.plugin_registry.as_deref(),
                    ctx.parent_compat,
                )
                .await,
        );
    }
    let skills_inherited_count = if inherit_skills {
        ctx.parent_skills.as_ref().map(|s| s.len() as u32).unwrap_or(0)
    } else {
        0
    };
    if skills_inherited_count > 0 {
        tracing::info!(
            subagent_id = % request.id, skills_count = skills_inherited_count,
            "Subagent inherited skills from parent"
        );
    }
    let mcp_owned_count = agent_mcp_servers.len() as u32;
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::SubagentLaunched {
        subagent_id: request.id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        subagent_type: request.subagent_type.clone(),
        persona: request.runtime_overrides.persona.clone(),
        fork_context: matches!(context_source, InitialContextSource::Forked),
        resume_from: request.resume_from.clone(),
        isolated_worktree: worktree_path.is_some(),
        mcp_inherited_count,
        mcp_owned_count,
        skills_inherited_count,
    });
    let subagent_session_default_agent_profile = Some(definition.name.clone());
    let subagent_model_id = effective_sampling_config.model.clone();
    let _ = persistence
        .tx
        .send(crate::session::persistence::PersistenceMsg::CurrentModel {
            model_id: effective_model_id.clone(),
            agent_name: Some(definition.name.clone()),
            reasoning_effort: Some(effective_sampling_config.reasoning_effort),
        });
    let forked_tool_override = if verbatim_mirror_fork && !request.owner.is_workflow() {
        ctx.parent_tool_snapshot.clone()
    } else {
        None
    };
    let spawn_result = session::spawn_session_on_thread(
            child_session_info,
            gateway.clone(),
            effective_sampling_config,
            credentials,
            crate::agent::auth_method::new_shared_auth_method_id(
                Some(ctx.auth_method_id.clone()),
            ),
            Some(ctx.auth_manager.clone()),
            attribution_callback,
            tool_ctx,
            agent_mcp_servers,
            vec![],
            Default::default(),
            parent_mcp_pool,
            Vec::new(),
            true,
            false,
            None,
            persistence,
            forked_conversation,
            None,
            None,
            initial_child_tokens,
            crate::session::StartupHints {
                inherited_prefix_len: Some(inherited_prefix_len),
                is_subagent: true,
                parent_session_id: Some(ctx.parent_session_id.clone()),
                subagent_type: Some(request.subagent_type.clone()),
                preserve_inherited_system: verbatim_mirror_fork,
                ..Default::default()
            },
            xai_grok_workspace::permission::ClientType::Generic,
            ctx.resolve_auto_compact_threshold_percent(&subagent_model_id),
            xai_grok_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_string(),
            xai_chat_state::CompactionMode::Summary,
            ctx.resolve_compaction_verbatim_input(),
            ctx.resolve_compaction_tool_choice(),
            false,
            None,
            None,
            std::sync::Arc::new(
                parking_lot::Mutex::new(
                    xai_grok_workspace::file_system::CodebaseIndexManager::new(),
                ),
            ),
            false,
            subagent_fs_watch,
            None,
            None,
            None,
            None,
            false,
            false,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            definition,
            subagent_session_default_agent_profile,
            if inherit_skills {
                ctx.parent_skills_config.clone()
            } else {
                xai_grok_agent::prompt::skills::SkillsConfig::default()
            },
            if inherit_skills { ctx.parent_skills.take() } else { None },
            ctx.parent_compat,
            false,
            None,
            None,
            None,
            Vec::new(),
            None,
            if verbatim_mirror_fork {
                None
            } else if let Some(scope) = agent_memory_scope {
                ctx.memory_config
                    .as_ref()
                    .map(|mc| {
                        let mut c = mc.clone();
                        let resolved = scope
                            .resolve_dir(&agent_name_for_memory, &ctx.parent_cwd);
                        c.enabled = true;
                        c.root_dir_override = Some(resolved.path);
                        c.flat_memory_root = resolved.is_project_scoped;
                        c
                    })
            } else {
                ctx.memory_config.clone()
            },
            false,
            Default::default(),
            ctx.managed_mcp_state.clone(),
            None,
            ctx.managed_mcp_proxy_base_url.clone(),
            effective_model_id,
            ctx.yolo_mode
                || matches!(
                    agent_permission_mode,
                    xai_grok_agent::config::PermissionMode::BypassPermissions
                ),
            false,
            None,
            ctx.inference_idle_timeout_secs,
            None,
            ctx.web_search_sampling_config.clone(),
            ctx.web_fetch_config.clone(),
            ctx.image_gen_config.clone(),
            ctx.video_gen_config.clone(),
            ctx.app_builder_deployer_config.clone(),
            ctx.write_file_enabled,
            ctx.goal_enabled,
            ctx.background_workflows_enabled,
            true,
            ctx.ask_user_question_enabled,
            ctx.client_hooks.clone(),
            None,
            std::collections::HashMap::new(),
            Vec::new(),
            xai_grok_agent::prompt::context::PromptAudience::Subagent,
            effective_runtime.role_prompt.clone(),
            None,
            ctx.disable_web_search,
            ctx.backend_tools_enabled,
            ctx.respect_gitignore,
            ctx.path_not_found_hints,
            ctx.resolve_tool_params_json(),
            ctx.plugin_registry.clone(),
            None,
            ctx.models_manager.clone(),
            parent_traceparent,
            ctx.permission_handle.clone(),
            ctx.api_key_provider.clone(),
            ctx.image_description_model.clone(),
            ctx.hook_registry.clone(),
            ctx.workspace_ops.clone(),
            vec![],
            ctx.todo_gate,
            std::mem::take(&mut ctx.remote_settings),
            std::mem::take(&mut ctx.laziness_debug_log),
            ctx.parent_terminal_backend.clone(),
            if request.owner.is_workflow() {
                None
            } else {
                ctx.parent_scheduler_handle.clone()
            },
            subagent_max_turns,
            forked_tool_override,
        )
        .await;
    let (child_handle, mut permission_rx, _system_prompt, child_thread) = match spawn_result {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Failed to spawn child session: {e}");
            pending_guard.set_error(msg.clone());
            fail_subagent(
                request,
                &msg,
                &subagent_id,
                &child_session_id,
                &subagent_meta_dir,
                gateway,
                &ctx.parent_session_id,
                ctx.parent_cmd_tx.as_ref(),
                start.elapsed().as_millis() as u64,
                &gcs_upload_ctx,
            );
            return;
        }
    };
    if cancel_token.is_cancelled() {
        pending_guard.defuse();
        ctx.workspace_ops.end_local_session(child_session_id.0.as_ref());
        cancel_pending_subagent_at_promote(
                request,
                &child_handle,
                &subagent_id,
                &child_session_id,
                &subagent_meta_dir,
                coordinator,
                gateway,
                &ctx.parent_session_id,
                ctx.parent_cmd_tx.as_ref(),
                worktree_path.as_deref(),
                worktree_freshly_created,
                start.elapsed().as_millis() as u64,
                &gcs_upload_ctx,
            )
            .await;
        return;
    }
    pending_guard.defuse();
    coordinator
        .borrow_mut()
        .insert(SubagentTracker {
            subagent_id: request.id.clone(),
            parent_session_id: ctx.parent_session_id.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
            owner: request.owner.clone(),
            child_session_id: child_session_id.clone(),
            subagent_type: request.subagent_type.clone(),
            persona: effective_runtime.persona.clone(),
            description: request.description.clone(),
            started_at: start,
            child_handle: child_handle.clone(),
            child_thread,
            cancel_token: cancel_token.clone(),
            resumed_from: request.resume_from.clone(),
            child_cwd: tracker_child_cwd,
            worktree_path: worktree_path.clone(),
            effective_model_id: tracker_model_id,
            run_in_background,
            surface_completion: request.surface_completion,
            completion_output_cap: request.runtime_overrides.completion_output_cap,
            color: tracker_color,
            block_waited: false,
            explicitly_killed: false,
        });
    spawn_progress_publisher(
        child_handle.signals_handle.clone(),
        gateway.clone(),
        ctx.parent_session_id.clone(),
        request.id.clone(),
        child_session_id.0.to_string(),
        start,
        cancel_token.clone(),
        goal_tick_cmd_tx(ctx.goal_enabled, ctx.parent_cmd_tx.as_ref()),
    );
    let (before_copy_tx, before_copy_rx) = tokio::sync::oneshot::channel();
    let _ = child_handle
        .cmd_tx
        .send(SessionCommand::CopyFile {
            respond_to: before_copy_tx,
        });
    let (prompt_tx, prompt_rx) = oneshot::channel();
    let prompt_text = task_prompt_text;
    let child_prompt_id = uuid::Uuid::now_v7().to_string();
    let turn_started_at = chrono::Utc::now().to_rfc3339();
    let _ = child_handle
        .cmd_tx
        .send(SessionCommand::Prompt {
            prompt_id: child_prompt_id.clone(),
            prompt_blocks: vec![
                acp::ContentBlock::Text(acp::TextContent::new(prompt_text))
            ],
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            artifact_upload_ctx: ctx
                .gcs_bucket_url
                .as_ref()
                .and_then(|_| {
                    ctx
                        .gcs_upload_method
                        .as_ref()
                        .map(|method| crate::upload::manifest::ArtifactUploadContext {
                            gcs_config: crate::session::repo_changes::TraceExportConfig {
                                bucket_url: ctx.gcs_bucket_url.clone(),
                                service_account_key: None,
                                prefix_dir: None,
                                gcs_prefix: Some(format!("{}/turn_0", child_session_id.0)),
                                absolute_paths: false,
                                archive_name_override: None,
                                upload_method: method.clone(),
                            },
                            artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
                        })
                }),
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            traceparent: xai_file_utils::trace_context::current_traceparent(),
            json_schema: request.runtime_overrides.output_schema.clone(),
            send_now: false,
            admission: None,
            respond_to: prompt_tx,
            persist_ack: None,
            parsed_prompt_tx: None,
        });
    let mut result_tx = {
        let (dummy_tx, _) = oneshot::channel();
        Some(std::mem::replace(&mut request.result_tx, dummy_tx))
    };
    let wait_outcome = {
        let fut = await_subagent_turn_or_cancellation(prompt_rx, cancel_token.clone());
        tokio::pin!(fut);
        if !request.run_in_background {
            /// How the bounded foreground wait ended.
            enum ForegroundWait {
                /// The child finished (or was cancelled) within the budget.
                Done(SubagentWaitOutcome),
                /// The spawning tool's `result_rx` was dropped — parent turn died mid-await.
                ParentGone,
                /// `subagent_await_budget()` expired.
                Budget,
            }
            let first = {
                let parent_await_dropped = async {
                    match result_tx.as_mut() {
                        Some(tx) => tx.closed().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                let budget = async {
                    if request.await_to_completion {
                        std::future::pending::<()>().await
                    } else {
                        tokio::time::sleep(subagent_await_budget()).await
                    }
                };
                tokio::select! {
                    biased; outcome = & mut fut => ForegroundWait::Done(outcome), _ =
                    parent_await_dropped => ForegroundWait::ParentGone, _ = budget =>
                    ForegroundWait::Budget,
                }
            };
            match first {
                ForegroundWait::Done(outcome) => {
                    if matches!(outcome, SubagentWaitOutcome::Cancelled) {
                        parent_wait_guard.take();
                    }
                    outcome
                }
                ForegroundWait::ParentGone => {
                    parent_wait_guard.take();
                    if request.owner.is_workflow() {
                        tracing::info!(
                            subagent_id = % request.id, workflow_run_id = ? request.owner
                            .workflow_run_id(),
                            "workflow subagent result receiver dropped; cancelling child",
                        );
                        cancel_token.cancel();
                    } else {
                        tracing::info!(
                            subagent_id = % request.id,
                            "foreground subagent await abandoned by its parent turn; detaching child to background (child keeps running)",
                        );
                        if !cancel_token.is_cancelled() {
                            request.run_in_background = true;
                            coordinator.borrow_mut().mark_backgrounded(&request.id);
                        }
                    }
                    fut.await
                }
                ForegroundWait::Budget => {
                    tracing::info!(
                        subagent_id = % request.id, budget_ms = subagent_await_budget()
                        .as_millis() as u64,
                        "foreground subagent exceeded await budget; auto-backgrounding (child keeps running)",
                    );
                    if let Some(tx) = result_tx.take() {
                        let _ = tx
                            .send(SubagentResult {
                                backgrounded: true,
                                subagent_id: request.id.clone(),
                                child_session_id: child_session_id.0.to_string(),
                                ..Default::default()
                            });
                    }
                    parent_wait_guard.take();
                    request.run_in_background = true;
                    coordinator.borrow_mut().mark_backgrounded(&request.id);
                    fut.await
                }
            }
        } else {
            fut.await
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;
    let mut turn_token_totals: Option<(u64, u64, u64)> = None;
    let mut cancellation_may_hide_usage = false;
    let mut result = match wait_outcome {
        SubagentWaitOutcome::Cancelled => {
            let (tool_calls, turns) = signals_snapshot_counts(&child_handle).await;
            cancellation_may_hide_usage = turns > 0 || tool_calls > 0;
            SubagentResult {
                success: false,
                cancelled: true,
                error: Some("Subagent was cancelled".to_string()),
                subagent_id: request.id.clone(),
                child_session_id: child_session_id.0.to_string(),
                tool_calls,
                turns,
                duration_ms,
                worktree_path: worktree_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                ..Default::default()
            }
        }
        SubagentWaitOutcome::TurnResult(turn_result) => {
            let was_cancelled = cancel_token.is_cancelled();
            let (tool_calls, turns) = match &*turn_result {
                Ok(
                    Ok(
                        crate::session::commands::PromptTurnOk {
                            turn_snapshot: Some(snapshot),
                            ..
                        },
                    ),
                ) => {
                    turn_token_totals = Some((
                        snapshot.turn_input_tokens,
                        snapshot.turn_cached_input_tokens,
                        snapshot.turn_output_tokens,
                    ));
                    (snapshot.current.tool_call_count, snapshot.current.turn_count)
                }
                _ => signals_snapshot_counts(&child_handle).await,
            };
            let final_text = child_handle
                .chat_state_handle
                .get_last_assistant_text()
                .await
                .unwrap_or_default();
            let result_tokens = child_handle.chat_state_handle.get_total_tokens().await;
            match *turn_result {
                Ok(
                    Ok(
                        crate::session::commands::PromptTurnOk {
                            completion_kind: PromptCompletionKind::Cancelled {
                                category,
                                context,
                            },
                            ..
                        },
                    ),
                ) => {
                    cancellation_may_hide_usage = true;
                    let reason = cancellation_error_message(category, context.as_ref());
                    SubagentResult {
                        success: false,
                        cancelled: true,
                        error: Some(reason),
                        output: if final_text.is_empty() {
                            std::sync::Arc::from(
                                format!(
                                    "Subagent '{}' ({}) was cancelled. {} tool calls, {} turns.",
                                    request.description, request.subagent_type, tool_calls,
                                    turns
                                ),
                            )
                        } else {
                            std::sync::Arc::from(final_text)
                        },
                        subagent_id: request.id.clone(),
                        child_session_id: child_session_id.0.to_string(),
                        tool_calls,
                        turns,
                        duration_ms,
                        tokens_used: result_tokens,
                        output_tokens_used: 0,
                        output_usage_incomplete: true,
                        total_tokens_used: 0,
                        worktree_path: worktree_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        backgrounded: false,
                    }
                }
                Ok(
                    Ok(
                        crate::session::commands::PromptTurnOk {
                            completion_kind: PromptCompletionKind::MaxTurnsReached {
                                limit,
                            },
                            ..
                        },
                    ),
                ) => {
                    SubagentResult {
                        success: false,
                        cancelled: true,
                        error: Some(format!("max turns reached (limit: {limit})")),
                        output: if final_text.is_empty() {
                            std::sync::Arc::from(
                                format!(
                                    "Subagent '{}' ({}) hit max-turns limit ({limit}). {} tool calls, {} turns.",
                                    request.description, request.subagent_type, tool_calls,
                                    turns
                                ),
                            )
                        } else {
                            std::sync::Arc::from(final_text)
                        },
                        subagent_id: request.id.clone(),
                        child_session_id: child_session_id.0.to_string(),
                        tool_calls,
                        turns,
                        duration_ms,
                        tokens_used: result_tokens,
                        output_tokens_used: 0,
                        output_usage_incomplete: true,
                        total_tokens_used: 0,
                        worktree_path: worktree_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        backgrounded: false,
                    }
                }
                Ok(
                    Ok(crate::session::commands::PromptTurnOk { structured_output, .. }),
                ) => {
                    let wanted_schema = request
                        .runtime_overrides
                        .output_schema
                        .is_some();
                    let (success, error, output) = match (
                        wanted_schema,
                        structured_output,
                    ) {
                        (true, Some(Ok(value))) => {
                            (true, None, std::sync::Arc::from(value.to_string()))
                        }
                        (true, Some(Err(e))) => {
                            (
                                false,
                                Some(format!("structured output validation failed: {e}")),
                                std::sync::Arc::from(final_text),
                            )
                        }
                        (true, None) => {
                            (
                                false,
                                Some(
                                    "structured output requested but none produced".to_string(),
                                ),
                                std::sync::Arc::from(final_text),
                            )
                        }
                        (false, _) => {
                            (
                                true,
                                None,
                                if final_text.is_empty() {
                                    std::sync::Arc::from(
                                        format!(
                                            "Subagent '{}' ({}) completed successfully. {} tool calls, {} turns.",
                                            request.description, request.subagent_type, tool_calls,
                                            turns
                                        ),
                                    )
                                } else {
                                    std::sync::Arc::from(final_text)
                                },
                            )
                        }
                    };
                    SubagentResult {
                        success,
                        error,
                        output,
                        subagent_id: request.id.clone(),
                        child_session_id: child_session_id.0.to_string(),
                        tool_calls,
                        turns,
                        duration_ms,
                        tokens_used: result_tokens,
                        output_tokens_used: 0,
                        output_usage_incomplete: true,
                        total_tokens_used: 0,
                        worktree_path: worktree_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        ..Default::default()
                    }
                }
                Ok(Err(e)) => {
                    cancellation_may_hide_usage = was_cancelled;
                    SubagentResult {
                        success: false,
                        cancelled: was_cancelled,
                        error: Some(
                            if was_cancelled {
                                "Subagent was cancelled".to_string()
                            } else {
                                format!("Session error: {e}")
                            },
                        ),
                        subagent_id: request.id.clone(),
                        child_session_id: child_session_id.0.to_string(),
                        tool_calls,
                        turns,
                        duration_ms,
                        worktree_path: worktree_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        ..Default::default()
                    }
                }
                Err(_) => {
                    cancellation_may_hide_usage = was_cancelled;
                    SubagentResult {
                        success: false,
                        cancelled: was_cancelled,
                        error: Some(
                            if was_cancelled {
                                "Subagent was cancelled".to_string()
                            } else {
                                "Child session dropped unexpectedly".to_string()
                            },
                        ),
                        subagent_id: request.id.clone(),
                        child_session_id: child_session_id.0.to_string(),
                        tool_calls,
                        turns,
                        duration_ms,
                        worktree_path: worktree_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string()),
                        ..Default::default()
                    }
                }
            }
        }
    };
    if let Some(trace_gcs_config) = gcs_upload_ctx
        .upload_method
        .as_ref()
        .map(|method| crate::session::repo_changes::TraceExportConfig {
            bucket_url: gcs_upload_ctx.bucket_url.clone(),
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(format!("{}/turn_0", child_session_id.0)),
            absolute_paths: false,
            archive_name_override: None,
            upload_method: method.clone(),
        })
    {
        let (copy_tx, session_copy_rx) = tokio::sync::oneshot::channel();
        let _ = child_handle
            .cmd_tx
            .send(SessionCommand::CopyFile {
                respond_to: copy_tx,
            });
        let turn_messages: Option<xai_chat_state::TurnCapture> = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if child_handle
                .cmd_tx
                .send(SessionCommand::TakeTurnMessages {
                    respond_to: tx,
                })
                .is_ok()
            {
                rx.await.ok().flatten()
            } else {
                None
            }
        };
        let streaming_partial = crate::upload::turn::take_streaming_partial(
                &child_handle.cmd_tx,
                child_prompt_id.clone(),
                result.success,
                gcs_upload_ctx.model_id.clone(),
            )
            .await
            .map(|mut cap| {
                cap.reason = Some(
                    if result.cancelled {
                        "subagent_cancel".to_string()
                    } else {
                        "subagent_non_completed".to_string()
                    },
                );
                cap
            });
        let mut permission_events = Vec::new();
        while let Ok(event) = permission_rx.try_recv() {
            permission_events.push(event);
        }
        let trace_ctx = PromptTraceContext {
            gcs_config: trace_gcs_config,
            session_info: child_handle.info.clone(),
            turn_number: 0,
            session_handle: child_handle.clone(),
            session_registry_enabled: false,
            upload_queue: None,
            artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
            auth_manager: ctx.auth_manager.clone(),
        };
        let session_dir = crate::session::persistence::session_dir(&child_handle.info);
        if let Ok(prompt_bytes) = std::fs::read(session_dir.join("system_prompt.txt")) {
            let gcs_path = format!("{}/system_prompt.txt", child_session_id.0);
            crate::upload::trace::upload_trace_artifact(
                    &trace_ctx,
                    &prompt_bytes,
                    &gcs_path,
                    "text/plain",
                    "system_prompt",
                )
                .await;
        }
        if let Ok(ctx_bytes) = std::fs::read(session_dir.join("prompt_context.json")) {
            let gcs_path = format!("{}/prompt_context.json", child_session_id.0);
            crate::upload::trace::upload_trace_artifact(
                    &trace_ctx,
                    &ctx_bytes,
                    &gcs_path,
                    "application/json",
                    "prompt_context",
                )
                .await;
        }
        upload_session_state(
                &trace_ctx,
                "before",
                before_copy_rx,
                crate::upload::turn::UploadWait::Confirm,
            )
            .await;
        let subagent_auth = ctx.auth_manager.current();
        let metadata = PromptMetadata {
            schema_version: GCS_SCHEMA_VERSION.to_string(),
            session_id: child_session_id.0.to_string(),
            turn_number: 0,
            request_id: child_prompt_id.clone(),
            turn_started_at: turn_started_at.clone(),
            repo_root: None,
            remote_url: None,
            user_id: subagent_auth.as_ref().map(|a| a.user_id.clone()),
            user_email: subagent_auth.as_ref().and_then(|a| a.email.clone()),
            team_id: subagent_auth.as_ref().and_then(|a| a.team_id.clone()),
            client_source: Some("subagent".to_string()),
            client_version: ctx.sampling_config.client_version.clone(),
            model: gcs_upload_ctx.model_id.clone().unwrap_or_default(),
            reasoning_effort: child_handle
                .reasoning_effort
                .map(|e| e.as_str().to_string()),
            experiment_id: None,
            host_os: std::env::consts::OS.to_string(),
            host_arch: std::env::consts::ARCH.to_string(),
            prompt_has_image: Some(false),
            prompt_was_truncated: Some(false),
            prompt_verbatim: Some(true),
            cwd: Some(child_handle.info.cwd.clone()),
            agent_type: Some(request.subagent_type.clone()),
            shell_version: Some(xai_grok_version::VERSION.to_string()),
            workspace_type: None,
            sandbox: local_sandbox_telemetry(),
        };
        upload_metadata(&trace_ctx, metadata).await;
        let resolved_model = child_handle
            .get_model_metadata()
            .await
            .resolved_model_id
            .or_else(|| gcs_upload_ctx.model_id.clone());
        let turn_result_meta = TurnResultMetadata {
            schema_version: "1",
            request_id: child_prompt_id,
            completed: result.success,
            stop_reason: None,
            total_tokens: None,
            input_tokens: turn_token_totals.map(|t| t.0),
            cached_input_tokens: turn_token_totals.map(|t| t.1),
            output_tokens: turn_token_totals.map(|t| t.2),
            error: result.error.clone(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            signals: None,
            turn_delta: None,
            start_prompt_mode: Some(
                crate::session::plan_mode::PromptMode::Agent.to_string(),
            ),
            end_prompt_mode: Some(
                crate::session::plan_mode::PromptMode::Agent.to_string(),
            ),
            resolved_model,
            subagents_spawned: vec![],
        };
        upload_turn_result(
                &trace_ctx,
                &turn_result_meta,
                crate::upload::turn::UploadWait::Confirm,
            )
            .await;
        match complete_prompt_trace(
                trace_ctx,
                permission_events,
                session_copy_rx,
                turn_messages,
                streaming_partial,
                crate::upload::turn::UploadWait::Confirm,
            )
            .await
        {
            Ok(_) => {
                tracing::debug!(
                    subagent_id = % request.id, child_session_id = % child_session_id.0,
                    "Subagent trace artifacts uploaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    subagent_id = % request.id, error = % e,
                    "Subagent trace upload failed (non-fatal)"
                );
            }
        }
    }
    let persisted_output_dir = persist_subagent_output(&subagent_meta_dir, &result);
    persist_subagent_completion(&subagent_meta_dir, &result, &gcs_upload_ctx);
    let final_status = result.status().to_string();
    let snapshot_dispose_enabled = ctx.resolve_subagent_worktree_snapshot_enabled();
    let telemetry_tokens = if result.tool_calls > 0 || result.success {
        child_handle.chat_state_handle.get_total_tokens().await
    } else {
        0
    };
    let task_budget_usage = task_output_budget.as_ref().map(|budget| budget.usage());
    let (
        subagent_usage_by_model,
        subagent_usage_incomplete,
        output_tokens_used,
        total_tokens_used,
    ) = match child_handle.chat_state_handle.try_get_session_usage().await {
        Ok(u) => {
            let output_tokens = u.totals.output_tokens;
            let total_tokens = canonical_total_tokens(&u.totals);
            let has_usage_entries = !u.by_model.is_empty();
            let usage_incomplete = usage_is_incomplete(
                u.incomplete,
                cancellation_may_hide_usage,
                total_tokens,
                has_usage_entries,
            );
            (
                Some(u.by_model.into_iter().collect::<Vec<_>>()),
                usage_incomplete,
                (!usage_incomplete).then_some(output_tokens),
                Some(total_tokens),
            )
        }
        Err(()) => (None, true, None, None),
    };
    result.total_tokens_used = total_tokens_used.unwrap_or(0);
    if let Some((task_spent, task_incomplete)) = task_budget_usage {
        result.output_tokens_used = output_tokens_used.unwrap_or(task_spent);
        result.output_usage_incomplete = task_incomplete || subagent_usage_incomplete
            || output_tokens_used.is_none();
    } else {
        result.output_tokens_used = output_tokens_used.unwrap_or(0);
        result.output_usage_incomplete = subagent_usage_incomplete
            || output_tokens_used.is_none();
    }
    let fold_acked = match subagent_usage_by_model {
        None => false,
        Some(ref by_model) if by_model.is_empty() && !subagent_usage_incomplete => true,
        Some(by_model) => {
            if let Some(cmd_tx) = ctx.parent_cmd_tx.as_ref() {
                let (respond_to, ack) = tokio::sync::oneshot::channel();
                match cmd_tx
                    .send(crate::session::commands::SessionCommand::RecordSubagentUsage {
                        by_model,
                        parent_prompt_id: request.parent_prompt_id.clone(),
                        incomplete: subagent_usage_incomplete,
                        respond_to,
                    })
                {
                    Ok(()) => ack.await.is_ok(),
                    Err(_) => false,
                }
            } else {
                false
            }
        }
    };
    if !fold_acked {
        tracing::warn!(
            subagent_id = % request.id, parent_prompt_id = ? request.parent_prompt_id,
            "subagent usage not applied; parent bill marked incomplete"
        );
        let sticky_prompt = request
            .parent_prompt_id
            .clone()
            .or_else(|| coordinator.borrow().parent_prompt_id_for(&request.id));
        if let Some(cmd_tx) = ctx.parent_cmd_tx.as_ref() {
            let (respond_to, ack) = tokio::sync::oneshot::channel();
            if cmd_tx
                .send(crate::session::commands::SessionCommand::MarkSubagentUsageNotApplied {
                    parent_prompt_id: sticky_prompt,
                    respond_to,
                })
                .is_ok()
            {
                let _ = ack.await;
            }
        } else if let Some(ref pid) = sticky_prompt {
            coordinator.borrow_mut().mark_subagent_usage_not_applied(pid);
        }
    }
    let outcome = if result.success {
        xai_grok_telemetry::events::Outcome::Completed
    } else if result.cancelled {
        xai_grok_telemetry::events::Outcome::Cancelled
    } else {
        xai_grok_telemetry::events::Outcome::Error
    };
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::SubagentCompleted {
        subagent_id: request.id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        outcome,
        duration_ms: result.duration_ms,
        tool_calls: result.tool_calls,
        tokens_used: if telemetry_tokens > 0 { Some(telemetry_tokens) } else { None },
    });
    match (&ctx.parent_terminal_backend, &ctx.parent_notification_handle) {
        (Some(parent_tb), Some(parent_notif_handle)) => {
            if !request.surface_completion {
                let reparented_task_ids: Vec<String> = parent_tb
                    .list_tasks()
                    .await
                    .into_iter()
                    .filter(|t| {
                        !t.completed
                            && t.owner_session_id.as_deref()
                                == Some(&*child_session_id.0)
                    })
                    .map(|t| t.task_id)
                    .collect();
                if !reparented_task_ids.is_empty()
                    && let Some(cmd_tx) = ctx.parent_cmd_tx.as_ref()
                {
                    let _ = cmd_tx
                        .send(SessionCommand::RecordGoalTurnTaskIds {
                            task_ids: reparented_task_ids,
                        });
                }
            }
            let parent_backend_weak = std::sync::Arc::downgrade(parent_tb);
            parent_tb
                .reparent_notifications(
                    &child_session_id.0,
                    &ctx.parent_session_id,
                    parent_notif_handle.clone(),
                    parent_backend_weak,
                )
                .await;
        }
        (Some(_), None) | (None, Some(_)) => {
            tracing::warn!(
                child_session_id = % child_session_id.0, parent_session_id = % ctx
                .parent_session_id, has_terminal_backend = ctx.parent_terminal_backend
                .is_some(), has_notification_handle = ctx.parent_notification_handle
                .is_some(),
                "skipping reparent_notifications: parent_terminal_backend and \
                 parent_notification_handle must both be Some"
            );
        }
        (None, None) => {}
    }
    let _ = child_handle.cmd_tx.send(SessionCommand::Shutdown);
    ctx.workspace_ops.end_local_session(child_session_id.0.as_ref());
    let mut disposed_snapshot_ref: Option<String> = None;
    let mut worktree_removed = false;
    if let Some(ref wt_path) = worktree_path {
        if snapshot_dispose_enabled {
            let ref_name = format!("refs/grok/subagents/{}", request.id);
            let source_repo = resolve_subagent_source_repo(&ctx);
            match crate::session::worktree::snapshot_subagent_worktree(
                    wt_path,
                    &source_repo,
                    &ref_name,
                )
                .await
            {
                Ok(snapshot_ref) => {
                    let persisted = update_subagent_meta_snapshot_ref(
                        &subagent_meta_dir,
                        &snapshot_ref,
                        &final_status,
                    );
                    if persisted {
                        disposed_snapshot_ref = Some(snapshot_ref);
                        match crate::session::worktree::remove_subagent_worktree(wt_path)
                            .await
                        {
                            Ok(()) => {
                                worktree_removed = true;
                                tracing::info!(
                                    subagent_id = % request.id, worktree_path = % wt_path
                                    .display(), "snapshotted and removed subagent worktree"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    subagent_id = % request.id, worktree_path = % wt_path
                                    .display(), error = % e,
                                    "snapshotted subagent worktree but removal failed; ref persisted for resume"
                                )
                            }
                        }
                    } else {
                        tracing::warn!(
                            subagent_id = % request.id, worktree_path = % wt_path
                            .display(),
                            "snapshot_ref not persisted; preserving worktree for resume"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        subagent_id = % request.id, worktree_path = % wt_path.display(),
                        error = % e,
                        "Failed to snapshot subagent worktree; preserving for review"
                    );
                }
            }
        } else {
            tracing::info!(
                subagent_id = % request.id, worktree_path = % wt_path.display(),
                "Worktree preserved for review"
            );
        }
    }
    if worktree_removed {
        result.worktree_path = None;
    }
    let (block_waited, explicitly_killed) = {
        let mut coord = coordinator.borrow_mut();
        (
            coord.block_wait_delivered_or_live(&request.id),
            coord.is_explicitly_killed(&request.id),
        )
    };
    let will_wake = should_auto_wake_subagent(
        request.run_in_background,
        result.cancelled,
        ctx.auto_wake_enabled,
        block_waited,
        explicitly_killed,
        ctx.goal_loop_active.load(std::sync::atomic::Ordering::Relaxed),
        ctx.parent_cmd_tx.is_some(),
    );
    emit_subagent_notification(
        gateway,
        &ctx.parent_session_id,
        SessionUpdate::SubagentFinished {
            subagent_id: request.id.clone(),
            child_session_id: result.child_session_id.clone(),
            status: result.status().to_string(),
            error: result.error.clone(),
            tool_calls: result.tool_calls,
            turns: result.turns,
            duration_ms: result.duration_ms,
            tokens_used: telemetry_tokens,
            output: if result.success { Some(result.output.to_string()) } else { None },
            will_wake,
        },
        ctx.parent_cmd_tx.as_ref(),
    );
    coordinator
        .borrow_mut()
        .move_to_completed(
            &request.id,
            request.description.clone(),
            request.subagent_type.clone(),
            result.clone(),
            persisted_output_dir,
        );
    if let Some(snapshot_ref) = disposed_snapshot_ref {
        coordinator.borrow_mut().set_completed_snapshot_ref(&request.id, snapshot_ref);
    }
    if will_wake {
        inject_subagent_completed_prompt(
            &request.id,
            &result,
            &request,
            &ctx.task_completion_reservations,
            ctx.parent_cmd_tx.as_ref(),
            &ctx.task_output_tool_name,
            &ctx.synthetic_trace_tx,
        );
    }
    if let Some(tx) = result_tx.take() {
        let _ = tx.send(result);
    }
}
