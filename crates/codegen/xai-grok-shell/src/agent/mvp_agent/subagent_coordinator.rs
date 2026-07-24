//! Shell runner adapter and spawn-context construction for [`MvpAgent`].
//! The shared coordinator actor lives in `xai-grok-tools`; this module plugs
//! its `!Send` local-session runner into `spawn_local`.
use super::*;
use crate::session::repo_changes::UploadMethod;
struct ShellChildRunner {
    agent_ref: LocalRef<MvpAgent>,
}
impl xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunner
    for ShellChildRunner
{
    type Control = crate::agent::subagent::ShellChildRuntime;
    type CompletionData = crate::agent::subagent::ShellCompletionData;
    type RunFuture = xai_grok_tools::implementations::grok_build::task::coordinator::LocalBoxFuture<
        xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunOutput<
            Self::CompletionData,
        >,
    >;
    type ValidateFuture =
        xai_grok_tools::implementations::grok_build::task::coordinator::LocalBoxFuture<
            xai_grok_tools::implementations::grok_build::task::types::SubagentValidateTypeOutcome,
        >;
    type DescribeFuture =
        xai_grok_tools::implementations::grok_build::task::coordinator::LocalBoxFuture<
            xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome,
        >;
    fn run(
        &self,
        run: xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunRequest<
            Self::Control,
        >,
    ) -> Self::RunFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunRequest {
                request,
                cancellation,
                reporter,
            } = run;
            let this = agent_ref.get();
            let parent_sid = request.parent_session_id.clone();
            let Some(mut ctx) = this.try_build_subagent_spawn_context(&parent_sid) else {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    subagent_id = %request.id,
                    "Spawn for unknown or evicted parent session"
                );
                return xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunOutput {
                    result: xai_grok_tools::implementations::grok_build::task::types::SubagentResult {
                        success: false,
                        error: Some(
                            "Parent session not found (evicted or torn down); cannot spawn subagent."
                                .to_owned(),
                        ),
                        subagent_id: request.id.clone(),
                        child_session_id: request.id,
                        ..Default::default()
                    },
                    completion_data: Default::default(),
                    snapshot_ref: None,
                };
            };
            let parent_handle = {
                let parent_sid = acp::SessionId::new(parent_sid);
                this.sessions.borrow().get(&parent_sid).cloned()
            };
            if let Some(handle) = parent_handle {
                ctx.parent_mcp_pool = handle.snapshot_mcp_pool().await;
                ctx.client_hooks = handle.snapshot_client_hooks().await;
                let definitions = handle.snapshot_tool_definitions().await;
                ctx.parent_tool_definitions = (!definitions.is_empty()).then_some(definitions);
            }
            crate::agent::subagent::run_shell_child(
                request,
                ctx,
                cancellation,
                reporter,
                &this.gateway,
            )
            .await
        })
    }
    fn validate_type(
        &self,
        subagent_type: String,
        parent_session_id: String,
    ) -> Self::ValidateFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let this = agent_ref.get();
            let ctx = this.build_subagent_validation_context(&parent_session_id);
            crate::agent::subagent::validate_subagent_type(&subagent_type, &ctx)
        })
    }
    fn describe_type(
        &self,
        subagent_type: String,
        harness_agent_type: Option<String>,
        parent_session_id: String,
    ) -> Self::DescribeFuture {
        let agent_ref = self.agent_ref.clone();
        Box::pin(async move {
            let this = agent_ref.get();
            match this.try_build_subagent_spawn_context(&parent_session_id) {
                Some(ctx) => crate::agent::subagent::describe_subagent_type(
                    &subagent_type,
                    harness_agent_type.as_deref(),
                    &ctx,
                ),
                None => {
                    tracing::warn!(
                        parent_session_id,
                        subagent_type,
                        "DescribeType for unknown/evicted parent session, replying Unavailable",
                    );
                    xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome::Unavailable
                }
            }
        })
    }
    fn on_completed(
        &self,
        completion: xai_grok_tools::implementations::grok_build::task::coordinator::ChildCompletion<
            Self::CompletionData,
        >,
    ) {
        let gateway = self.agent_ref.get().gateway.clone();
        crate::agent::subagent::present_child_completion(completion, &gateway);
    }
    fn running_count_changed(&self, running: usize) {
        self.agent_ref
            .get()
            .activity
            .subagent_gauge()
            .store(running, std::sync::atomic::Ordering::Relaxed);
    }
    fn persisted_output_ref(&self, completion_data: &Self::CompletionData) -> Option<String> {
        completion_data
            .persisted_output_dir()
            .map(|path| path.to_string_lossy().into_owned())
    }
    fn load_persisted_output(&self, reference: &str) -> Option<std::sync::Arc<str>> {
        crate::agent::subagent::read_subagent_output(std::path::Path::new(reference))
            .map(std::sync::Arc::from)
    }
}
impl MvpAgent {
    /// Start the shared subagent coordinator actor.
    ///
    /// Takes `subagent_event_rx` once and `spawn_local`s one
    /// [`SubagentCoordinator`](xai_grok_tools::implementations::grok_build::task::coordinator::SubagentCoordinator)
    /// that drains `ChannelBackend` events (`Spawn` / await / cancel / inspect)
    /// through [`ShellChildRunner`]. The actor owns pending/active/completed
    /// state, waiters, deadlines, and completion disposition; the runner only
    /// builds shell child sessions via `run_shell_child`.
    ///
    /// Uses `LocalRef` so the `!Send` runner can touch `self` from the
    /// `LocalSet`. Idempotent: subsequent calls are no-ops.
    pub(super) fn start_subagent_coordinator(&self) {
        let Some(rx) = self.subagent_event_rx.borrow_mut().take() else {
            return;
        };
        let agent_ref = LocalRef::new(self);
        let runner = ShellChildRunner {
            agent_ref: agent_ref.clone(),
        };
        let config =
            xai_grok_tools::implementations::grok_build::task::coordinator::CoordinatorConfig {
                foreground_budget:
                    xai_grok_tools::implementations::grok_build::task::backend::env_duration_or(
                        "GROK_SUBAGENT_AWAIT_BUDGET_MS",
                        std::time::Duration::from_secs(600),
                    ),
                buffer_completions: true,
                buffered_completion_output_cap: None,
            };
        tokio::task::spawn_local(
            xai_grok_tools::implementations::grok_build::task::coordinator::SubagentCoordinator::new(
                    rx,
                    runner,
                    config,
                )
                .run(),
        );
        let (trace_tx, mut trace_rx) = tokio::sync::mpsc::unbounded_channel::<
            crate::upload::turn::SyntheticTurnTraceRequest,
        >();
        self.subagent_presentation.borrow_mut().synthetic_trace_tx = Some(trace_tx);
        tokio::task::spawn_local({
            let agent_ref = agent_ref.clone();
            async move {
                while let Some(request) = trace_rx.recv().await {
                    tokio::task::spawn_local({
                        let agent_ref = agent_ref.clone();
                        async move {
                            handle_synthetic_turn_trace(agent_ref, request).await;
                        }
                    });
                }
            }
        });
    }
    /// Lightweight context for the `SubagentEvent::ValidateType` drain arm;
    /// tolerates evicted parent sessions (returns built-in defaults + warns).
    pub(super) fn build_subagent_validation_context(
        &self,
        parent_session_id: &str,
    ) -> crate::agent::subagent::SubagentValidationContext {
        let parent_sid = acp::SessionId::new(parent_session_id);
        let (parent_cwd, allowed_subagent_types) = {
            let sessions = self.sessions.borrow();
            let ps = sessions.get(&parent_sid);
            warn_on_missing_parent_session_for_validate_type(parent_session_id, ps.is_some());
            (
                ps.map(|h| std::path::PathBuf::from(&h.info.cwd))
                    .unwrap_or_default(),
                ps.and_then(|h| h.allowed_subagent_types.clone()),
            )
        };
        let (cli_agent_names, subagent_toggle) = {
            let cfg = self.cfg.borrow();
            (
                cfg.cli_agents.iter().map(|d| d.name.clone()).collect(),
                cfg.subagent_toggle.clone(),
            )
        };
        crate::agent::subagent::SubagentValidationContext {
            parent_cwd,
            plugin_registry: self.plugin_registry_handle.snapshot(),
            subagent_toggle,
            allowed_subagent_types,
            cli_agent_names,
        }
    }
    /// Test-only infallible wrapper around
    /// [`Self::try_build_subagent_spawn_context`]. Production spawn paths use
    /// the fallible variant and fail the request when the parent session is
    /// absent (evicted, or a child-session spawn whose re-parent lookup
    /// missed).
    #[cfg(test)]
    pub(super) fn build_subagent_spawn_context(
        &self,
        parent_session_id: &str,
    ) -> crate::agent::subagent::SubagentSpawnContext {
        self.try_build_subagent_spawn_context(parent_session_id)
            .expect("parent session must exist when spawning subagents")
    }
    /// Build a `SubagentSpawnContext` from the current agent state and the
    /// parent session's shared resources. Returns `None` when the parent
    /// `SessionHandle` is absent (evicted / torn down) so callers can fail
    /// the request instead of panicking.
    ///
    /// This is the ONLY subagent-related method on MvpAgent besides the
    /// coordinator startup.
    pub(super) fn try_build_subagent_spawn_context(
        &self,
        parent_session_id: &str,
    ) -> Option<crate::agent::subagent::SubagentSpawnContext> {
        let parent_sid = acp::SessionId::new(parent_session_id);
        let (
            parent_model_id,
            parent_chat_state,
            parent_cmd_tx,
            parent_cwd,
            yolo_mode,
            parent_depth,
            hunk_tracker_handle,
            hunk_tracking_enabled,
            fs,
            terminal,
            session_env,
            parent_attribution_callback,
            parent_agent_name,
            parent_managed_mcp_proxy_base_url,
        ) = {
            let sessions = self.sessions.borrow();
            let ps = sessions.get(&parent_sid);
            (
                ps.map(|h| h.model_id.clone())
                    .unwrap_or_else(|| self.models_manager.current_model_id()),
                ps.map(|h| h.chat_state_handle.clone()),
                ps.map(|h| h.cmd_tx.clone()),
                ps.map(|h| std::path::PathBuf::from(&h.info.cwd))
                    .unwrap_or_default(),
                ps.map(|h| h.yolo_mode).unwrap_or(self.default_yolo_mode),
                ps.map(|h| h.tool_context.subagent_depth).unwrap_or(0),
                ps.map(|h| h.tool_context.hunk_tracker_handle.clone())
                    .unwrap_or_else(xai_hunk_tracker::HunkTrackerHandle::noop),
                ps.map(|h| h.tool_context.hunk_tracking_enabled)
                    .unwrap_or(false),
                ps.map(|h| h.tool_context.fs.inner().clone())
                    .unwrap_or_else(|| {
                        let cwd = ps
                            .map(|h| std::path::PathBuf::from(&h.info.cwd))
                            .unwrap_or_default();
                        std::sync::Arc::new(xai_grok_workspace::file_system::LocalFs::new(cwd))
                    }),
                ps.map(|h| h.tool_context.terminal.clone())
                    .unwrap_or_else(|| {
                        std::sync::Arc::new(crate::terminal::TerminalRunner::new(
                            std::sync::Arc::new(self.gateway.clone()),
                            parent_sid.clone(),
                        ))
                    }),
                ps.map(|h| h.tool_context.session_env.clone())
                    .unwrap_or_else(|| std::sync::Arc::new(std::collections::HashMap::new())),
                ps.and_then(|h| h.attribution_callback.clone()),
                ps.map(|h| h.agent_name.clone()),
                ps.map(|h| h.managed_mcp_proxy_base_url.clone()),
            )
        };
        let (
            parent_workspace_ops,
            parent_terminal_backend,
            parent_notification_handle,
            parent_scheduler_handle,
        ) = {
            let sessions = self.sessions.borrow();
            sessions.get(&parent_sid).map(|ps| {
                (
                    ps.workspace_ops.clone(),
                    ps.terminal_backend.clone(),
                    ps.tools_notification_handle.clone(),
                    ps.scheduler_handle.clone(),
                )
            })
        }?;
        let available_models = self.models_manager.models();
        let parent_lsp = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&parent_sid)
                .and_then(|h| h.tool_context.lsp.clone())
        };
        let am = self.auth_manager.clone();
        let inference_idle_timeout_secs = {
            let per_model = config::find_model_by_id(&available_models, parent_model_id.0.as_ref())
                .and_then(|e| e.info.inference_idle_timeout_secs);
            let cfg = self.cfg.borrow();
            let remote = cfg
                .remote_settings
                .as_ref()
                .and_then(|s| s.inference_idle_timeout_secs);
            per_model.or(remote).unwrap_or(600).max(10)
        };
        let parent_hook_registry = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&parent_sid)
                .and_then(|h| h.hook_registry.clone())
        };
        let parent_max_turns = {
            let sessions = self.sessions.borrow();
            sessions.get(&parent_sid).and_then(|h| h.max_turns)
        };
        let parent_model_agent_type =
            config::find_model_by_id(&available_models, parent_model_id.0.as_ref())
                .map(|e| e.info.agent_type.clone());
        let ask_user_question_enabled = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&parent_sid)
                .map(|h| h.ask_user_question_enabled)
                .unwrap_or_else(|| self.cfg.borrow().resolve_ask_user_question().value)
        };
        let (gcs_upload_method, gcs_bucket_url) = match self.trace_upload_config_snapshot() {
            Some(method) => {
                let bucket = match &method {
                    UploadMethod::Direct { .. } => self
                        .cfg
                        .borrow()
                        .endpoints
                        .resolve_trace_bucket_url()
                        .map(|r| r.value),
                    UploadMethod::Proxy { .. } => Some("proxy-managed".to_string()),
                    UploadMethod::S3 { bucket, .. } => Some(format!("s3://{bucket}")),
                };
                match bucket {
                    Some(url) => (Some(method), Some(url)),
                    None => (None, None),
                }
            }
            None => (None, None),
        };
        let project_trusted = crate::agent::folder_trust::project_scope_allowed(&parent_cwd);
        let (base_roles, base_personas, subagent_model_overrides, subagent_toggle) = {
            let cfg = self.cfg.borrow();
            (
                cfg.subagent_roles.clone(),
                cfg.subagent_personas.clone(),
                cfg.subagent_model_overrides.clone(),
                cfg.subagent_toggle.clone(),
            )
        };
        let (subagent_roles, subagent_personas) =
            crate::config::SubagentsConfig::effective_definition_maps(
                &base_roles,
                &base_personas,
                &parent_cwd,
                project_trusted,
            );
        let inherited_tool_overrides = {
            let sessions = self.sessions.borrow();
            sessions
                .get(&parent_sid)
                .and_then(|ps| ps.resolved_tool_overrides.load_full().map(|o| (*o).clone()))
        };
        Some(crate::agent::subagent::SubagentSpawnContext {
            lsp: parent_lsp,
            client_hooks: Default::default(),
            sampling_config: self.sampling_config.borrow().clone(),
            managed_mcp_proxy_base_url: parent_managed_mcp_proxy_base_url
                .unwrap_or_else(|| self.cli_chat_proxy_base_url()),
            alpha_test_key: self.alpha_test_key(),
            auth_method_id: self
                .auth_method_id
                .load()
                .as_deref()
                .cloned()
                .unwrap_or_else(|| acp::AuthMethodId::new("default")),
            model_id: parent_model_id,
            auth: self.current_or_buffered_auth(),
            parent_cwd: parent_cwd.clone(),
            parent_session_id: parent_session_id.to_string(),
            inherited_tool_overrides,
            yolo_mode,
            subagent_event_tx: self.subagent_event_tx.clone(),
            parent_depth,
            inference_idle_timeout_secs,
            auto_compact_threshold_tiers:
                crate::agent::subagent::AutoCompactThresholdTiers::capture(&self.cfg.borrow()),
            hunk_tracker_handle,
            hunk_tracking_enabled,
            fs,
            terminal,
            session_env,
            memory_config: self.memory_config.clone(),
            web_search_sampling_config: self.prepare_web_search_sampling_config(),
            web_fetch_config: self.prepare_web_fetch_config(),
            image_gen_config: self.prepare_image_gen_config(),
            video_gen_config: self.prepare_video_gen_config(),
            app_builder_deployer_config: self.prepare_app_builder_deployer_config(),
            write_file_enabled: self.cfg.borrow().resolve_write_file().value,
            goal_enabled: self.cfg.borrow().resolve_goal().value,
            background_workflows_enabled: self.cfg.borrow().resolve_workflows().value,
            ask_user_question_enabled,
            parent_cmd_tx: parent_cmd_tx.clone(),
            parent_session_info: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .map(|h| crate::session::info::Info {
                        id: parent_sid.clone(),
                        cwd: h.info.cwd.clone(),
                    })
            },
            parent_chat_state,
            parent_max_turns,
            available_models,
            subagent_model_overrides,
            subagent_toggle,
            subagent_roles,
            subagent_personas,
            disable_web_search: self.cfg.borrow().disable_web_search,
            todo_gate: self.cfg.borrow().todo_gate,
            remote_settings: self.cfg.borrow().remote_settings.clone(),
            laziness_debug_log: self.cfg.borrow().laziness_debug_log.clone(),
            backend_tools_enabled: self.cfg.borrow().resolve_backend_tools().value,
            respect_gitignore: self.cfg.borrow().respect_gitignore,
            path_not_found_hints: self.cfg.borrow().path_not_found_hints,
            plugin_registry: self.plugin_registry_handle.snapshot(),
            models_manager: self.models_manager.clone(),
            file_tool_overrides: {
                let cfg = self.cfg.borrow();
                let effective = cfg
                    .toolset
                    .resolve_file_toolset(cfg.remote_settings.as_ref());
                if effective != crate::tools::FileToolset::Standard {
                    effective.tool_configs(&cfg.toolset.hashline).ok()
                } else {
                    None
                }
            },
            gcs_bucket_url,
            agent_config: Some(self.cfg.borrow().clone()),
            gcs_upload_method,
            hook_registry: parent_hook_registry,
            permission_handle: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .map(|h| h.permission_handle.clone())
            },
            worktree_type: self.worktree_type,
            api_key_provider: Some(Arc::new(crate::auth::manager::SharedAuthKeyProvider(
                am.clone(),
            ))),
            image_description_model: self.resolve_image_description_model(),
            workspace_ops: parent_workspace_ops.clone(),
            auth_manager: am.clone(),
            attribution_callback: parent_attribution_callback,
            parent_agent_name,
            parent_model_agent_type,
            allowed_subagent_types: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .and_then(|h| h.allowed_subagent_types.clone())
            },
            parent_mcp_configs: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .map(|h| h.mcp_servers.clone())
                    .unwrap_or_default()
            },
            managed_mcp_state: self.managed_mcp_cache.clone(),
            parent_mcp_pool: None,
            parent_tool_definitions: None,
            parent_skills: None,
            parent_skills_config: self.cfg.borrow().skills.clone(),
            parent_compat: self.cfg.borrow().compat_resolved,
            task_completion_reservations: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .and_then(|h| h.tool_context.task_completion_reservations.clone())
            },
            synthetic_trace_tx: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .and_then(|h| h.tool_context.synthetic_trace_tx.clone())
            },
            task_output_tool_name: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .map(|h| h.tool_context.task_output_tool_name.clone())
                    .unwrap_or_else(|| {
                        xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL
                            .to_string()
                    })
            },
            auto_wake_enabled: self.cfg.borrow().auto_wake_enabled,
            goal_loop_active: {
                let sessions = self.sessions.borrow();
                sessions
                    .get(&parent_sid)
                    .map(|h| h.tool_context.goal_loop_active_gate.clone())
                    .unwrap_or_else(|| {
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
                    })
            },
            parent_terminal_backend: parent_terminal_backend.clone(),
            parent_notification_handle: parent_notification_handle.clone(),
            parent_scheduler_handle: parent_scheduler_handle.clone(),
        })
    }
}
