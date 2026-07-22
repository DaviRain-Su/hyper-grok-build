//! Notification bridge: translates `xai-grok-tools` `ToolNotification` events
//! into `xai-grok-shell`'s native systems (ACP gateway, hunk tracker, file state tracker).
use crate::session::commands::SessionCommand;
use crate::session::commands::{NotificationPriority, NotificationSource};
use crate::session::persistence::{DurableAppendError, PersistenceHandle, PersistenceMsg};
use agent_client_protocol::{self as acp, Client as _};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex as TokioMutex, mpsc};
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_tools::notification::types::{ToolNotification, ToolNotificationHandle};
use xai_grok_tools::types::output::{BashOutput, ToolOutput};
use xai_grok_workspace::session::file_state::FileStateTracker;
use xai_hunk_tracker::HunkTrackerHandle;
const TASK_WAKE_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
/// Configuration for the notification bridge.
pub struct NotificationBridgeConfig {
    /// ACP gateway for sending streaming updates to TUI
    pub gateway: GatewaySender,
    /// ACP session ID
    pub session_id: acp::SessionId,
    /// Hunk tracker for recording agent writes
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// File state tracker for rewind functionality
    pub file_state_tracker: Arc<FileStateTracker>,
    /// Current prompt index (shared with session state)
    pub prompt_index: Arc<TokioMutex<usize>>,
    /// Working directory for path relativization
    pub cwd: PathBuf,
    /// Shared gate: when false, suppress gateway forwarding.
    /// Events are still processed for hunk tracking and file state.
    pub gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Persistence handle for FIFO ordinary writes and durable tombstone barriers.
    pub persistence: PersistenceHandle,
    /// When true, send incremental `output_delta` instead of full `output`
    /// in bash streaming updates. The client must opt in via the
    /// `x.ai/incrementalBashOutput` capability.
    pub incremental_bash_output: bool,
    /// Plan mode tracker shared with the session actor.
    /// Used to transition state on `PlanModeEntered` / `PlanModeExited`
    /// tool notifications.
    pub plan_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PlanModeTracker>>,
    /// Session-level prompt mode shared with the session actor.
    /// Updated on `PlanModeEntered` / `PlanModeExited` and `session/set_mode`
    /// so the next turn starts in the correct mode.
    pub current_prompt_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PromptMode>>,
    /// Turn-level prompt mode. Set at turn start, then updated only by
    /// agent tool calls (`EnterPlanMode` / `ExitPlanMode`). NOT affected
    /// by `session/set_mode`. Read at turn end for `end_prompt_mode`.
    pub turn_prompt_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PromptMode>>,
    /// Session command channel for monitor events and task-completed injections.
    pub session_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub task_completion_reservations:
        xai_grok_tools::reminders::task_completion::TaskCompletionReservations,
    pub task_wake_suppressed: xai_grok_tools::reminders::task_completion::TaskWakeSuppressed,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    /// Wrapped in `Arc<Mutex<..>>` because the coordinator creates the channel
    /// after the notification bridge is spawned — the bridge reads the latest
    /// value on each notification.
    pub(crate) synthetic_trace_tx: Arc<
        std::sync::Mutex<
            Option<
                tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>,
            >,
        >,
    >,
    /// Resolved name of the `BackgroundTaskAction` tool. Written exactly
    /// once after the agent's toolset is finalized; read many times
    /// thereafter from the notification bridge and the session actor's
    /// between-turn drain. `None` means no such tool is registered in this
    /// toolset, which is a valid resolved state.
    pub task_output_tool_name: Arc<std::sync::OnceLock<Option<String>>>,
    /// Resolved name of the `Read` tool, used by `format_bash_completion`'s
    /// disk-pointer footer so the model can recover full bash output from
    /// `task.output_file` even when no polling tool is available. Same
    /// write-once-read-many lifecycle as `task_output_tool_name`.
    pub read_tool_name: Arc<std::sync::OnceLock<Option<String>>>,
    /// When `false`, bash task completions fall back to the idle-gated
    /// `InjectNotification` path instead of immediate synthetic prompts.
    pub auto_wake_enabled: bool,
    /// When `true`, an approved `PlanModeExited` also arms the tracker's
    /// next-turn exit reminder. Grok-build leaves this `false` — its
    /// exit-plan tool result already informs the model, and a deferred
    /// reminder would arrive stale. Shared with the session actor (the
    /// `gateway_enabled` pattern) and refreshed on zero-turn rebuilds so the
    /// bridge always agrees with the live session gate.
    pub queue_exit_reminder_on_approved_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When `true`, suppress the bash auto-wake synthetic prompt. Shared `Arc`
    /// written at one chokepoint — see
    /// `SessionActor::set_goal_loop_active_resource` for the rationale.
    pub goal_loop_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
/// Snapshot a shared `OnceLock` tool-name slot as a borrowed `&str`.
/// Returns `None` if the slot is still unset (toolset not yet finalized)
/// or if the resolved value is `None` (no such tool registered in this
/// toolset.
pub(crate) fn resolved_tool_name(slot: &std::sync::OnceLock<Option<String>>) -> Option<&str> {
    slot.get().and_then(|v| v.as_deref())
}
/// Stamp a bridge-emitted notification's meta before it forks into
/// persistence + broadcast — see `util::event_id::ensure_event_id_meta`.
fn stamp_event_id(config: &NotificationBridgeConfig, meta: &mut Option<acp::Meta>) {
    crate::util::event_id::ensure_event_id_meta(&config.session_id.0, meta);
}
fn stamp_scheduler_meta(
    config: &NotificationBridgeConfig,
    meta: &mut Option<acp::Meta>,
    generation: &str,
    revision: u64,
) {
    stamp_event_id(config, meta);
    let meta = meta.get_or_insert_with(acp::Meta::new);
    meta.insert("x.ai/schedulerGeneration".to_owned(), generation.into());
    meta.insert("x.ai/schedulerRevision".to_owned(), revision.into());
}
fn durable_append_landed(result: Result<(), DurableAppendError>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(DurableAppendError::Committed(error)) => {
            tracing::warn!(
                % error, "Scheduler tombstone committed with bookkeeping failure"
            );
            Ok(())
        }
        Err(DurableAppendError::NotCommitted(error)) => {
            Err(format!("scheduler tombstone was not committed: {error}"))
        }
        Err(DurableAppendError::AcknowledgementLost(error)) => Err(format!(
            "scheduler tombstone commit status is unknown: {error}"
        )),
    }
}
async fn handle_scheduled_task_removed(
    config: &NotificationBridgeConfig,
    removed: xai_grok_tools::notification::ScheduledTaskRemoved,
    acknowledgement: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    tracing::info!(task_id = % removed.task_id, "Scheduled task removed");
    let result: Result<Box<serde_json::value::RawValue>, String> = async {
        let mut meta = None;
        stamp_scheduler_meta(config, &mut meta, &removed.generation, removed.revision);
        let notification = crate::extensions::notification::SessionNotification {
            session_id: config.session_id.clone(),
            update: crate::extensions::notification::SessionUpdate::ScheduledTaskDeleted {
                task_id: removed.task_id,
            },
            meta: meta.map(serde_json::Value::Object),
        };
        let params = serde_json::to_value(&notification)
            .and_then(|value| serde_json::value::to_raw_value(&value))
            .map_err(|error| format!("failed to serialize scheduled task deletion: {error}"))?;
        let update = crate::session::storage::SessionUpdate::Xai(Box::new(notification));
        if acknowledgement.is_some() {
            durable_append_landed(config.persistence.append_update_durably(update).await)?;
        } else {
            config
                .persistence
                .tx
                .send(PersistenceMsg::Update(update))
                .map_err(|_| "session persistence stopped".to_owned())?;
        }
        Ok(params)
    }
    .await;
    match result {
        Ok(params) => {
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement.send(Ok(()));
            }
            config
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/scheduled_task_deleted",
                    params.into(),
                ));
            Ok(())
        }
        Err(error) => {
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement.send(Err(error.clone()));
            }
            Err(error)
        }
    }
}
/// Create a `ToolNotificationHandle` and spawn a bridge task that
/// translates notifications into shell-native systems.
pub fn spawn_notification_bridge(config: NotificationBridgeConfig) -> ToolNotificationHandle {
    let (handle, mut rx) = ToolNotificationHandle::acknowledged_channel();
    tokio::task::spawn_local(async move {
        let mut offsets: HashMap<String, usize> = HashMap::new();
        while let Some(delivery) = rx.recv().await {
            let acknowledgement = delivery.acknowledgement;
            match delivery.notification {
                ToolNotification::ScheduledTaskRemoved(removed) => {
                    if let Err(error) =
                        handle_scheduled_task_removed(&config, removed, acknowledgement).await
                    {
                        tracing::warn!(
                            % error, "Failed to handle scheduled task removal"
                        );
                    }
                }
                notification => {
                    handle_notification(&config, notification, &mut offsets).await;
                    if let Some(acknowledgement) = acknowledgement {
                        let _ = acknowledgement.send(Ok(()));
                    }
                }
            }
        }
        tracing::debug!("Notification bridge task exiting (sender dropped)");
    });
    handle
}
/// Emit a `CurrentModeUpdate` for the given [`SessionMode`] — persisted to
/// `updates.jsonl` so session replay re-applies the mode, and forwarded to
/// the gateway so the pager updates live.
async fn emit_current_mode_update(
    config: &NotificationBridgeConfig,
    mode: xai_grok_tools::types::SessionMode,
) {
    let mut notification = acp::SessionNotification::new(
        config.session_id.clone(),
        acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
            acp::SessionModeId::new(mode.as_id()),
        )),
    );
    stamp_event_id(config, &mut notification.meta);
    let _ = config.persistence.tx.send(PersistenceMsg::Update(
        crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
    ));
    config.gateway.forward_fire_and_forget(notification);
}
/// Handle a single notification by forwarding it to the appropriate shell system.
async fn handle_notification(
    config: &NotificationBridgeConfig,
    notification: ToolNotification,
    offsets: &mut HashMap<String, usize>,
) {
    match notification {
        ToolNotification::BashOutputChunk(chunk) => {
            let (output, output_delta) = if config.incremental_bash_output {
                let prev_offset = offsets.get(&chunk.base.tool_call_id).copied().unwrap_or(0);
                let full = &chunk.base.output;
                let delta = if prev_offset <= full.len() {
                    full[prev_offset..].to_vec()
                } else {
                    full.clone()
                };
                offsets.insert(chunk.base.tool_call_id.clone(), full.len());
                (Vec::new(), Some(delta))
            } else {
                (chunk.base.output.clone(), None)
            };
            let bash_output = ToolOutput::Bash(BashOutput {
                output_for_prompt: BashOutput::make_output_for_prompt(&String::from_utf8_lossy(
                    &chunk.base.output,
                )),
                output,
                exit_code: 0,
                command: chunk.base.command.clone(),
                truncated: chunk.base.truncated,
                signal: None,
                timed_out: false,
                description: None,
                current_dir: chunk.base.cwd.to_string_lossy().to_string(),
                output_file: String::new(),
                total_bytes: chunk.base.total_bytes,
                output_delta,
                was_bare_echo: false,
            });
            let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(chunk.base.tool_call_id.clone()),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::InProgress))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(
                            String::from_utf8_lossy(&chunk.base.output).into_owned(),
                        )),
                    )]))
                    .raw_output(serde_json::to_value(&bash_output).ok()),
            ));
            let mut notification = acp::SessionNotification::new(config.session_id.clone(), update);
            stamp_event_id(config, &mut notification.meta);
            let _ = config.persistence.tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
            ));
            if config
                .gateway_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let _ = config.gateway.session_notification(notification).await;
            }
        }
        ToolNotification::BashExecutionComplete(complete) => {
            offsets.remove(&complete.base.tool_call_id);
            tracing::debug!(
                tool_call_id = % complete.base.tool_call_id, exit_code = ? complete
                .exit_code, "Bash execution complete notification received"
            );
        }
        ToolNotification::BashExecutionTimeout(timeout) => {
            tracing::debug!(
                tool_call_id = % timeout.base.tool_call_id, elapsed = ? timeout.elapsed,
                "Bash execution timeout notification received"
            );
        }
        ToolNotification::BashExecutionFailed(failed) => {
            tracing::warn!(
                tool_call_id = % failed.tool_call_id, error = % failed.error,
                "Bash execution failed notification received"
            );
        }
        ToolNotification::BashExecutionBackgrounded(bg) => {
            tracing::debug!(
                tool_call_id = % bg.base.tool_call_id, task_id = % bg.task_id, command =
                % bg.base.command, output_file = % bg.output_file.display(),
                "Bash execution backgrounded notification received — forwarding to TUI"
            );
            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::TaskBackgrounded {
                    tool_call_id: bg.base.tool_call_id.clone(),
                    task_id: bg.task_id.clone(),
                    command: bg.base.command.clone(),
                    cwd: bg.base.cwd.to_string_lossy().to_string(),
                    output_file: bg.output_file.to_string_lossy().to_string(),
                    monitor_description: bg.monitor_description.clone(),
                    description: bg.description.clone(),
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }
            let _ = config.persistence.tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                let ext_notification =
                    acp::ExtNotification::new("x.ai/task_backgrounded", params.into());
                config.gateway.forward_fire_and_forget(ext_notification);
            }
        }
        ToolNotification::FileWritten(written) => {
            let prompt_index = *config.prompt_index.lock().await;
            config.hunk_tracker_handle.record_agent_write(
                written.absolute_path.clone(),
                written.content.clone(),
                prompt_index,
                written.previous_content.clone(),
            );
            if written.previous_content.is_some() || written.is_new_file {
                config
                    .file_state_tracker
                    .add_before_snapshot_for_prompt(
                        prompt_index,
                        &written.absolute_path,
                        &config.cwd,
                        written.previous_content,
                    )
                    .await;
            }
            tracing::debug!(
                path = % written.absolute_path.display(), is_new_file = written
                .is_new_file, "FileWritten notification forwarded to hunk tracker"
            );
        }
        ToolNotification::TaskCompleted(task_snapshot) => {
            let is_monitor =
                task_snapshot.kind == xai_grok_tools::computer::types::TaskKind::Monitor;
            let task_id = task_snapshot.task_id.clone();
            let goal_loop_active = config
                .goal_loop_active
                .load(std::sync::atomic::Ordering::Relaxed);
            let mut will_wake = false;
            if task_snapshot.block_waited || task_snapshot.explicitly_killed {
            } else if goal_loop_active {
                tracing::info!(
                    task_id = % task_id, is_monitor,
                    "auto-wake: suppressed completion (goal loop active)"
                );
            } else if config.auto_wake_enabled {
                config.task_completion_reservations.reserve(task_id.clone());
                let tool_name = resolved_tool_name(&config.task_output_tool_name);
                let read_name = resolved_tool_name(&config.read_tool_name);
                let body = if is_monitor {
                    xai_grok_tools::reminders::task_completion::format_monitor_completion(
                        &task_snapshot,
                        tool_name,
                    )
                } else {
                    xai_grok_tools::reminders::task_completion::format_bash_completion(
                        &task_snapshot,
                        tool_name,
                        read_name,
                    )
                };
                let message = xai_grok_tools::reminders::wrap_reminder(&body);
                let prompt_id = format!("task-completed-{task_id}");
                let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(message))];
                let synthetic_trace_tx = config
                    .synthetic_trace_tx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let (respond_to, completion_rx) = tokio::sync::oneshot::channel();
                let (admission_tx, admission_rx) = tokio::sync::oneshot::channel();
                tracing::info!(
                    task_id = % task_id, prompt_id = % prompt_id, is_monitor,
                    "auto-wake: requesting synthetic prompt admission for completed background task"
                );
                let enqueued = config
                    .session_cmd_tx
                    .send(SessionCommand::Prompt {
                        prompt_id: prompt_id.clone(),
                        prompt_blocks,
                        prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                        artifact_upload_ctx: None,
                        client_identifier: None,
                        screen_mode: None,
                        verbatim: true,
                        traceparent: xai_file_utils::trace_context::current_traceparent(),
                        json_schema: None,
                        send_now: false,
                        admission: Some(crate::session::commands::TaskWakeAdmission {
                            respond_to: admission_tx,
                            fallback: crate::session::commands::TaskWakeFallback {
                                prompt_id: if is_monitor {
                                    format!("monitor-completed-{task_id}")
                                } else {
                                    format!("bash-completed-{task_id}")
                                },
                                prompt_blocks: vec![acp::ContentBlock::Text(
                                    acp::TextContent::new(body.clone()),
                                )],
                                source: if is_monitor {
                                    NotificationSource::MonitorCompleted {
                                        task_id: task_id.clone(),
                                    }
                                } else {
                                    NotificationSource::BashTaskCompleted {
                                        task_id: task_id.clone(),
                                    }
                                },
                            },
                        }),
                        respond_to,
                        persist_ack: None,
                        parsed_prompt_tx: None,
                    })
                    .is_ok();
                if !enqueued {
                    config.task_completion_reservations.release(&task_id);
                }
                let admitted = if enqueued {
                    tokio::time::timeout(TASK_WAKE_ADMISSION_TIMEOUT, admission_rx)
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(false)
                } else {
                    false
                };
                will_wake = admitted;
                xai_grok_telemetry::unified_log::info(
                    "shell.task_wake.bridge_admission",
                    Some(config.session_id.0.as_ref()),
                    Some(serde_json::json!(
                        { "task_id" : & task_id, "monitor" : is_monitor, "enqueued" :
                        enqueued, "admitted" : admitted, "gate" : config
                        .task_wake_suppressed.get(), }
                    )),
                );
                if will_wake {
                    if is_monitor {
                        let _ =
                            config
                                .session_cmd_tx
                                .send(SessionCommand::DropMonitorNotifications {
                                    task_id: task_id.clone(),
                                });
                    }
                    if let Some(trace_tx) = synthetic_trace_tx {
                        let (before_copy_tx, before_session_copy_rx) =
                            tokio::sync::oneshot::channel();
                        let copy_requested = config
                            .session_cmd_tx
                            .send(SessionCommand::CopyFile {
                                respond_to: before_copy_tx,
                            })
                            .is_ok();
                        if copy_requested {
                            tracing::info!(
                                task_id = % task_id,
                                "auto-wake: sending synthetic turn trace request"
                            );
                            let _ = trace_tx.send(crate::upload::turn::SyntheticTurnTraceRequest {
                                session_id: config.session_id.clone(),
                                prompt_id,
                                completion_rx,
                                before_session_copy_rx,
                            });
                        } else {
                            tracing::debug!(
                                task_id = % task_id,
                                "auto-wake: session snapshot request failed, skipping trace request"
                            );
                        }
                    } else {
                        tracing::debug!(
                            task_id = % task_id,
                            "auto-wake: no synthetic trace consumer, skipping trace request"
                        );
                    }
                }
            } else {
                let tool_name = resolved_tool_name(&config.task_output_tool_name);
                let read_name = resolved_tool_name(&config.read_tool_name);
                let message = if is_monitor {
                    xai_grok_tools::reminders::task_completion::format_monitor_completion(
                        &task_snapshot,
                        tool_name,
                    )
                } else {
                    xai_grok_tools::reminders::task_completion::format_bash_completion(
                        &task_snapshot,
                        tool_name,
                        read_name,
                    )
                };
                let source = if is_monitor {
                    NotificationSource::MonitorCompleted {
                        task_id: task_id.clone(),
                    }
                } else {
                    NotificationSource::BashTaskCompleted {
                        task_id: task_id.clone(),
                    }
                };
                let _ = config
                    .session_cmd_tx
                    .send(SessionCommand::InjectNotification {
                        prompt_id: if is_monitor {
                            format!("monitor-completed-{task_id}")
                        } else {
                            format!("bash-completed-{task_id}")
                        },
                        prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                            message,
                        ))],
                        priority: NotificationPriority::Later,
                        source,
                    });
            }
            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::TaskCompleted {
                    task_snapshot,
                    will_wake,
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }
            let _ = config.persistence.tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                let notification: acp::ExtNotification =
                    acp::ExtNotification::new("x.ai/task_completed", params.into());
                config.gateway.forward_fire_and_forget(notification);
            }
            let _ = config
                .session_cmd_tx
                .send(SessionCommand::DispatchNotificationHook {
                    notification_type: "task_complete".into(),
                    message: Some(format!("Background task completed: {task_id}")),
                    title: None,
                    level: Some("info".into()),
                });
        }
        ToolNotification::PlanModeEntered(entered) => {
            let activated = config.plan_mode.lock().activate_from_tool();
            if activated {
                *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
                *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
                let snapshot = config.plan_mode.lock().snapshot();
                let _ = config
                    .persistence
                    .tx
                    .send(PersistenceMsg::PlanModeState(snapshot));
                emit_current_mode_update(config, xai_grok_tools::types::SessionMode::Plan).await;
            }
            tracing::info!(
                tool_call_id = % entered.tool_call_id, activated,
                "Plan mode entered via EnterPlanMode tool"
            );
        }
        ToolNotification::PlanModeExited(exited) => {
            let deactivated = {
                let mut tracker = config.plan_mode.lock();
                let deactivated = tracker.deactivate_approved();
                if deactivated
                    && config
                        .queue_exit_reminder_on_approved_exit
                        .load(std::sync::atomic::Ordering::Relaxed)
                {
                    tracker.queue_exit_reminder();
                }
                deactivated
            };
            if deactivated {
                *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Agent;
                *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Agent;
                let snapshot = config.plan_mode.lock().snapshot();
                let _ = config
                    .persistence
                    .tx
                    .send(PersistenceMsg::PlanModeState(snapshot));
                emit_current_mode_update(config, xai_grok_tools::types::SessionMode::Default).await;
            }
            tracing::info!(
                tool_call_id = % exited.tool_call_id, deactivated, has_plan = exited
                .plan_content.is_some(), "Plan mode exited via ExitPlanMode tool"
            );
        }
        ToolNotification::UserQuestionAsked(asked) => {
            tracing::info!(tool_call_id = % asked.tool_call_id, "User question asked");
        }
        ToolNotification::LspServerStarting(s) => {
            tracing::debug!(
                server = % s.server_name, command = % s.command, "LSP server starting"
            );
        }
        ToolNotification::LspServerReady(s) => {
            tracing::info!(server = % s.server_name, "LSP server ready");
        }
        ToolNotification::LspServerCrashed(s) => {
            tracing::warn!(server = % s.server_name, "LSP server crashed");
        }
        ToolNotification::LspServerRetrying(s) => {
            tracing::warn!(
                server = % s.server_name, attempt = s.attempt, max_restarts = s
                .max_restarts, backoff_ms = s.backoff_ms, "LSP server retrying"
            );
        }
        ToolNotification::LspServerFailed(s) => {
            tracing::error!(
                server = % s.server_name, error = % s.error, "LSP server failed"
            );
        }
        ToolNotification::ScheduledTaskFired(fired) => {
            tracing::info!(
                task_id = % fired.task_id, schedule = % fired.human_schedule, subagent_id
                = fired.subagent_id.as_deref().unwrap_or(""), "Scheduled task fired"
            );
            if fired.subagent_id.is_none() {
                let inject_payload = serde_json::json!(
                    { "sessionId" : config.session_id, "taskId" : & fired.task_id,
                    "prompt" : & fired.prompt, "humanSchedule" : & fired.human_schedule,
                    "nextFireAt" : & fired.next_fire_at, }
                );
                if let Ok(params) = serde_json::value::to_raw_value(&inject_payload) {
                    config
                        .gateway
                        .forward_fire_and_forget(acp::ExtNotification::new(
                            "x.ai/scheduled_task_inject_prompt",
                            params.into(),
                        ));
                }
            }
            let mut meta = None;
            stamp_scheduler_meta(config, &mut meta, &fired.generation, fired.revision);
            let fired_notif = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::ScheduledTaskFired {
                    task_id: fired.task_id,
                    prompt: fired.prompt,
                    human_schedule: fired.human_schedule,
                    next_fire_at: fired.next_fire_at,
                    subagent_id: fired.subagent_id,
                },
                meta: meta.map(serde_json::Value::Object),
            };
            if let Ok(params) =
                serde_json::to_value(&fired_notif).and_then(|v| serde_json::value::to_raw_value(&v))
            {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_fired",
                        params.into(),
                    ));
            }
        }
        ToolNotification::MonitorEvent(event) => {
            let my_session = config.session_id.0.as_ref();
            if let Some(owner) = event.owner_session_id.as_deref()
                && owner != my_session
            {
                tracing::warn!(
                    task_id = % event.task_id, description = % event.description,
                    monitor_owner = % owner, bridge_session = % my_session,
                    "Dropped cross-session monitor event: owner does not match this bridge's session"
                );
                return;
            }
            tracing::debug!(
                task_id = % event.task_id, description = % event.description,
                "Monitor event received, injecting into session"
            );
            let notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::MonitorEvent {
                    task_id: event.task_id.clone(),
                    description: event.description.clone(),
                    event_text: event.raw_text.clone(),
                },
                meta: None,
            };
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/monitor_event",
                        params.into(),
                    ));
            }
            if config.task_completion_reservations.contains(&event.task_id) {
                tracing::debug!(
                    task_id = % event.task_id,
                    "skipping model inject for monitor event: task already auto-woke via TaskCompleted"
                );
                return;
            }
            let prompt_id = format!("monitor-{}-{}", event.task_id, uuid::Uuid::now_v7());
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                event.event_text,
            ))];
            let _ = config
                .session_cmd_tx
                .send(SessionCommand::InjectNotification {
                    prompt_id,
                    prompt_blocks,
                    priority: NotificationPriority::Next,
                    source: NotificationSource::MonitorEvent {
                        task_id: event.task_id.clone(),
                    },
                });
        }
        ToolNotification::ScheduledTaskRemoved(removed) => {
            if let Err(error) = handle_scheduled_task_removed(config, removed, None).await {
                tracing::warn!(% error, "Failed to handle scheduled task removal");
            }
        }
        ToolNotification::ScheduledTaskCreated(created) => {
            tracing::info!(task_id = % created.task_id, "Scheduled task created");
            let mut meta = None;
            stamp_scheduler_meta(config, &mut meta, &created.generation, created.revision);
            let notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::ScheduledTaskCreated {
                    task_id: created.task_id,
                    prompt: created.prompt,
                    human_schedule: created.human_schedule,
                    next_fire_at: created.next_fire_at,
                },
                meta: meta.map(serde_json::Value::Object),
            };
            let _ = config.persistence.tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
            if let Ok(params) = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
            {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_created",
                        params.into(),
                    ));
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::computer::types::TaskKind;
    use xai_grok_tools::types::TaskSnapshot;
    /// Drive the admission handshake inline so receiver assertions observe the
    /// bridge's command order without racing a detached proxy task.
    async fn handle_notification_with_admission(
        config: &NotificationBridgeConfig,
        notification: ToolNotification,
        offsets: &mut HashMap<String, usize>,
        cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>,
        accepted: bool,
    ) {
        let notification = handle_notification(config, notification, offsets);
        tokio::pin!(notification);
        let mut command = tokio::select! {
            _ = & mut notification =>
            panic!("notification completed before requesting admission"), command =
            cmd_rx.recv() => command.expect("expected task-wake prompt"),
        };
        let SessionCommand::Prompt { admission, .. } = &mut command else {
            panic!("expected task-wake prompt");
        };
        admission
            .take()
            .expect("expected task-wake admission request")
            .respond_to
            .send(accepted)
            .expect("notification must still be awaiting admission");
        config
            .session_cmd_tx
            .send(command)
            .expect("test command receiver must remain open");
        notification.await;
    }
    fn make_test_config() -> (
        NotificationBridgeConfig,
        mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        let (config, _gateway_rx, _persistence_rx, session_cmd_rx) = make_test_config_full();
        (config, session_cmd_rx)
    }
    #[allow(clippy::type_complexity)]
    fn make_test_config_full() -> (
        NotificationBridgeConfig,
        mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
        mpsc::UnboundedReceiver<PersistenceMsg>,
        mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        make_test_config_full_raw()
    }
    #[allow(clippy::type_complexity)]
    fn make_test_config_full_raw() -> (
        NotificationBridgeConfig,
        mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
        mpsc::UnboundedReceiver<PersistenceMsg>,
        mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
        let gateway = xai_acp_lib::AcpAgentGatewaySender::new(gateway_tx);
        let (session_cmd_tx, session_cmd_rx) = mpsc::unbounded_channel();
        let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
        let config = NotificationBridgeConfig {
            gateway,
            session_id: acp::SessionId::new("test-session"),
            hunk_tracker_handle: HunkTrackerHandle::noop(),
            file_state_tracker: Arc::new(FileStateTracker::new()),
            prompt_index: Arc::new(TokioMutex::new(0)),
            cwd: PathBuf::from("/tmp"),
            gateway_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            persistence: PersistenceHandle::from_sender_for_test(persistence_tx),
            incremental_bash_output: false,
            plan_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PlanModeTracker::new(PathBuf::from("/tmp/test-session")),
            )),
            current_prompt_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PromptMode::Agent,
            )),
            turn_prompt_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PromptMode::Agent,
            )),
            session_cmd_tx,
            task_completion_reservations:
                xai_grok_tools::reminders::task_completion::TaskCompletionReservations::default(),
            task_wake_suppressed:
                xai_grok_tools::reminders::task_completion::TaskWakeSuppressed::default(),
            synthetic_trace_tx: Arc::new(std::sync::Mutex::new(None)),
            task_output_tool_name: Arc::new(std::sync::OnceLock::new()),
            read_tool_name: Arc::new(std::sync::OnceLock::new()),
            auto_wake_enabled: true,
            queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            goal_loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (config, gateway_rx, persistence_rx, session_cmd_rx)
    }
    fn make_task_snapshot(task_id: &str, kind: TaskKind) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.into(),
            command: "echo test".into(),
            display_command: None,
            cwd: String::new(),
            start_time: std::time::SystemTime::now(),
            end_time: Some(std::time::SystemTime::now()),
            output: String::new(),
            output_file: PathBuf::new(),
            truncated: false,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind,
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: None,
        }
    }
    #[tokio::test]
    async fn bash_task_completed_injects_bash_task_completed_source() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let snapshot = make_task_snapshot("bg-123", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(&config, notification, &mut offsets, &mut cmd_rx, true)
            .await;
        let command = cmd_rx.try_recv().expect("expected Prompt");
        match command {
            SessionCommand::Prompt {
                prompt_id,
                prompt_blocks,
                verbatim,
                ..
            } => {
                assert!(prompt_id.starts_with("task-completed-"));
                assert!(verbatim);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    _ => panic!("expected text block"),
                };
                assert!(text.contains("bg-123"));
                assert!(text.contains("exit code: 0"));
                assert!(text.contains(r#"get_command_or_subagent_output("bg-123")"#));
                assert!(!text.contains(r#"get_task_output("bg-123")"#));
            }
            _ => panic!("expected Prompt"),
        }
        let cmd3 = cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete");
        match cmd3 {
            SessionCommand::DispatchNotificationHook {
                notification_type,
                message,
                ..
            } => {
                assert_eq!(notification_type, "task_complete");
                assert_eq!(
                    message.as_deref(),
                    Some("Background task completed: bg-123")
                );
            }
            _ => panic!("expected DispatchNotificationHook"),
        }
    }
    /// Gap 1: while a goal loop is active, a completed background bash task
    /// must NOT fire the synthetic auto-wake prompt — an async "task completed"
    /// wake mid-goal derails a weak model. It must also NOT be marked
    /// reserved (so surface 2's `TaskCompletionReminder` is free to
    /// drain it). The pager's `x.ai/task_completed` notification still fires.
    #[tokio::test]
    async fn bash_task_completed_suppresses_auto_wake_during_goal_loop() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("bg-goal", TaskKind::Bash);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active bash completion must not inject auto-wake commands"
        );
        assert!(
            config.task_completion_reservations.snapshot().is_empty(),
            "goal-loop-active completion must not be marked reserved"
        );
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }
    /// Gap 1 (preserve non-goal behavior): with the goal loop inactive — the
    /// default for a normal session — a completed bash task DOES fire the
    /// synthetic auto-wake prompt AND is marked reserved so surface
    /// 2 suppresses the duplicate reminder.
    #[tokio::test]
    async fn bash_task_completed_auto_wakes_and_reserves_without_goal_loop() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let snapshot = make_task_snapshot("bg-normal", TaskKind::Bash);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
            &mut cmd_rx,
            true,
        )
        .await;
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::Prompt { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        assert_eq!(
            config.task_completion_reservations.snapshot(),
            vec!["bg-normal".to_string()],
        );
    }
    fn task_completed_will_wake(
        gateway_rx: &mut mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
    ) -> Option<bool> {
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                let v: serde_json::Value = serde_json::from_str(args.request.params.get()).ok()?;
                return v["update"]["will_wake"].as_bool();
            }
        }
        None
    }
    /// The completion notification carries the wake verdict — the pager keys
    /// its between-turns status line on it (skip when a wake response
    /// follows, emit when nothing else will mark the moment).
    #[tokio::test]
    async fn task_completed_notification_stamps_will_wake() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
        *config
            .synthetic_trace_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(trace_tx);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-wake", TaskKind::Bash)),
            &mut offsets,
            &mut cmd_rx,
            true,
        )
        .await;
        assert!(matches!(
            cmd_rx.recv().await,
            Some(SessionCommand::Prompt { .. })
        ));
        match cmd_rx.recv().await {
            Some(SessionCommand::CopyFile { respond_to }) => drop(respond_to),
            _ => panic!("trace copy must follow accepted prompt admission"),
        }
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(true),
            "an auto-woken completion must stamp will_wake: true"
        );
        assert!(
            trace_rx.try_recv().is_ok(),
            "accepted admission must request a synthetic-turn trace"
        );
        let (config, mut gateway_rx, mut persistence_rx, mut cmd_rx) = make_test_config_full();
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
        *config
            .synthetic_trace_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(trace_tx);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-declined", TaskKind::Bash)),
            &mut offsets,
            &mut cmd_rx,
            false,
        )
        .await;
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(false),
            "an actor-declined completion must stamp will_wake: false"
        );
        assert!(
            config.task_completion_reservations.contains("bg-declined"),
            "the actor owns reservation release after queuing the deferred fallback"
        );
        assert!(
            trace_rx.try_recv().is_err(),
            "declined admission must not request a synthetic-turn trace"
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::Prompt { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        let mut persisted = false;
        while let Ok(message) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(update)) =
                message
                && matches!(
                    &update.update,
                    crate::extensions::notification::SessionUpdate::TaskCompleted { .. }
                )
            {
                persisted = true;
            }
        }
        assert!(
            persisted,
            "declined admission must still persist x.ai/task_completed"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn stalled_admission_is_bounded_and_task_completion_still_emits() {
        let (config, mut gateway_rx, mut persistence_rx, mut cmd_rx) = make_test_config_full_raw();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut offsets = HashMap::new();
        let notification = handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-stalled", TaskKind::Bash)),
            &mut offsets,
        );
        tokio::pin!(notification);
        tokio::select! {
            _ = & mut notification => panic!("admission should still be waiting"),
            command = cmd_rx.recv() => assert!(matches!(command,
            Some(SessionCommand::Prompt { .. }))),
        }
        tokio::time::advance(TASK_WAKE_ADMISSION_TIMEOUT + std::time::Duration::from_millis(1))
            .await;
        tokio::task::yield_now().await;
        notification.await;
        assert_eq!(task_completed_will_wake(&mut gateway_rx), Some(false));
        assert!(
            config.task_completion_reservations.contains("bg-stalled"),
            "a timed-out admission may still be handled and deferred by the actor"
        );
        let mut persisted_completion = false;
        while let Ok(message) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(update)) =
                message
                && matches!(
                    &update.update,
                    crate::extensions::notification::SessionUpdate::TaskCompleted { .. }
                )
            {
                persisted_completion = true;
            }
        }
        assert!(persisted_completion);
    }
    #[tokio::test(start_paused = true)]
    async fn timed_out_monitor_admission_queues_one_fallback_and_late_actor_drops_prompt() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full_raw();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut offsets = HashMap::new();
        let notification = handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("mon-timeout", TaskKind::Monitor)),
            &mut offsets,
        );
        tokio::pin!(notification);
        let prompt = tokio::select! {
            _ = & mut notification => panic!("admission should still be waiting"),
            command = cmd_rx.recv() => command.expect("prompt command"),
        };
        tokio::time::advance(TASK_WAKE_ADMISSION_TIMEOUT + std::time::Duration::from_millis(1))
            .await;
        tokio::task::yield_now().await;
        notification.await;
        let SessionCommand::Prompt {
            admission: Some(admission),
            respond_to,
            ..
        } = prompt
        else {
            panic!("expected task wake prompt");
        };
        assert!(
            matches!(admission.fallback.source, NotificationSource::MonitorCompleted {
            ref task_id } if task_id == "mon-timeout")
        );
        assert!(admission.respond_to.send(true).is_err());
        let _ = respond_to.send(Ok(crate::session::commands::PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens: 0,
            turn_snapshot: None,
            completion_kind: crate::session::commands::PromptCompletionKind::RemovedFromQueue,
            structured_output: None,
            usage: None,
        }));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(task_completed_will_wake(&mut gateway_rx), Some(false));
        assert!(
            config.task_completion_reservations.contains("mon-timeout"),
            "the late actor fallback retains the reservation until user delivery"
        );
    }
    #[tokio::test]
    async fn task_completed_stamps_will_wake_false_when_session_channel_closed() {
        let (config, mut gateway_rx, _persistence_rx, cmd_rx) = make_test_config_full_raw();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        drop(cmd_rx);
        config
            .task_completion_reservations
            .reserve("bg-dead".into());
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-dead", TaskKind::Bash)),
            &mut offsets,
        )
        .await;
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(false),
            "a completion whose wake prompt could not be enqueued must stamp will_wake: false"
        );
        assert!(config.task_completion_reservations.contains("bg-dead"));
        config.task_completion_reservations.release("bg-dead");
        assert!(!config.task_completion_reservations.contains("bg-dead"));
    }
    /// Gap 1 (adjacent branch): the goal-loop arm sits BEFORE the
    /// `auto_wake_enabled == false` `InjectNotification` fallback, so an
    /// auto-wake-DISABLED completion mid-goal must also be suppressed — it must
    /// NOT fall through to the idle-gated `InjectNotification`. Guards against a
    /// future reorder that would leak a mid-goal notification.
    #[tokio::test]
    async fn bash_task_completed_auto_wake_disabled_still_suppressed_during_goal_loop() {
        let (mut config, mut cmd_rx) = make_test_config();
        config.auto_wake_enabled = false;
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("bg-disabled-goal", TaskKind::Bash);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active completion must not InjectNotification with auto-wake disabled"
        );
        assert!(config.task_completion_reservations.snapshot().is_empty());
    }
    /// Natural monitor exit (including exit code 0) must immediate-auto-wake
    /// the same way bash does — not only via the idle-gated MonitorEvent path.
    /// Also drops queued MonitorEvents so a second NotificationDrain turn is
    /// not started for the same completion.
    #[tokio::test]
    async fn monitor_task_completed_auto_wakes_with_monitor_ended_message() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut snapshot = make_task_snapshot("mon-456", TaskKind::Monitor);
        snapshot.display_command = Some("[monitor] watch deploy".into());
        snapshot.command = "tail -f deploy.log".into();
        snapshot.exit_code = Some(0);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
            &mut cmd_rx,
            true,
        )
        .await;
        let cmd = cmd_rx.try_recv().expect("expected Prompt auto-wake");
        match cmd {
            SessionCommand::Prompt {
                prompt_id,
                prompt_blocks,
                verbatim,
                ..
            } => {
                assert_eq!(prompt_id, "task-completed-mon-456");
                assert!(verbatim);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => t.text.as_str(),
                    _ => panic!("expected text block"),
                };
                assert!(
                    text.contains("[monitor ended: exited (code 0)]"),
                    "auto-wake must carry the terminal ended wording: {text}"
                );
                assert!(
                    text.contains("watch deploy"),
                    "auto-wake should include the monitor description: {text}"
                );
                assert!(
                    text.contains("get_command_or_subagent_output(\"mon-456\")"),
                    "auto-wake should point at the poll tool: {text}"
                );
            }
            _ => panic!("expected Prompt auto-wake for natural monitor exit"),
        }
        match cmd_rx
            .try_recv()
            .expect("accepted monitor wake must drop pipeline notifications")
        {
            SessionCommand::DropMonitorNotifications { task_id } => {
                assert_eq!(task_id, "mon-456");
            }
            _ => panic!("expected DropMonitorNotifications after accepted Prompt"),
        }
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        assert_eq!(
            config.task_completion_reservations.snapshot(),
            vec!["mon-456".to_string()],
        );
    }
    #[tokio::test]
    async fn declined_quiet_monitor_wake_queues_canonical_deferred_completion() {
        let (config, _gateway_rx, mut persistence_rx, mut cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("mon-declined", TaskKind::Monitor)),
            &mut offsets,
            &mut cmd_rx,
            false,
        )
        .await;
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::Prompt { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        assert!(cmd_rx.try_recv().is_err());
        let mut persisted_completion = false;
        while let Ok(message) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(update)) =
                message
                && matches!(
                    &update.update,
                    crate::extensions::notification::SessionUpdate::TaskCompleted { .. }
                )
            {
                persisted_completion = true;
            }
        }
        assert!(persisted_completion);
        assert!(
            config.task_completion_reservations.contains("mon-declined"),
            "the actor owns reservation release after queuing the deferred fallback"
        );
    }
    /// After TaskCompleted auto-wake reserves the task, late pipeline
    /// MonitorEvents must not inject another model-facing notification.
    #[tokio::test]
    async fn monitor_event_skipped_after_task_completed_auto_wake() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_completion_reservations
            .reserve("mon-done".into());
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::MonitorEvent(xai_grok_tools::notification::types::MonitorEvent {
                task_id: "mon-done".into(),
                description: "short exit".into(),
                event_text: "<monitor-event>done</monitor-event>".into(),
                raw_text: "done".into(),
                owner_session_id: Some("test-session".into()),
            }),
            &mut offsets,
        )
        .await;
        assert!(
            cmd_rx.try_recv().is_err(),
            "post-auto-wake MonitorEvent must not InjectNotification"
        );
    }
    /// Explicit kill of a monitor still skips auto-wake — the model already
    /// got the kill_task tool result.
    #[tokio::test]
    async fn monitor_explicitly_killed_skips_auto_wake() {
        let (config, mut cmd_rx) = make_test_config();
        let mut snapshot = make_task_snapshot("mon-killed", TaskKind::Monitor);
        snapshot.explicitly_killed = true;
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "explicitly-killed monitor must not auto-wake"
        );
        assert!(config.task_completion_reservations.snapshot().is_empty());
    }
    /// Goal-loop suppression applies to monitor completions too.
    #[tokio::test]
    async fn monitor_task_completed_suppressed_during_goal_loop() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("mon-goal", TaskKind::Monitor);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active monitor completion must not auto-wake"
        );
        assert!(config.task_completion_reservations.snapshot().is_empty());
    }
    #[tokio::test]
    async fn scheduled_task_created_is_persisted() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::ScheduledTaskCreated(
            xai_grok_tools::notification::types::ScheduledTaskCreated {
                task_id: "loop-1".into(),
                prompt: "check deploy".into(),
                human_schedule: "every 5 minutes".into(),
                next_fire_at: Some("2026-01-01T00:00:00Z".into()),
                generation: "generation-a".into(),
                revision: 1,
            },
        );
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        let msg = persistence_rx
            .try_recv()
            .expect("scheduled_task_created must be persisted");
        match msg {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(matches!(
                    &notif.update,
                    crate::extensions::notification::SessionUpdate::ScheduledTaskCreated { .. }
                ));
                let meta = notif.meta.as_ref().expect("scheduler metadata");
                assert_eq!(meta["x.ai/schedulerGeneration"], "generation-a");
                assert_eq!(meta["x.ai/schedulerRevision"], 1);
                assert!(
                    notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id.starts_with("test-session-")),
                    "persisted xAI bridge lines must carry an eventId"
                );
            }
            _ => panic!("expected PersistenceMsg::Update(Xai(ScheduledTaskCreated))"),
        }
    }
    /// Persisted⇒stamped contract at the bridge's highest-frequency emitter:
    /// the persisted bash-output line carries an `eventId`, and the live
    /// broadcast carries the SAME id (the meta is minted before the
    /// persist/broadcast fork — divergent ids would re-deliver the line on a
    /// cursor reconnect).
    #[tokio::test]
    async fn bash_output_chunk_persists_and_broadcasts_one_event_id() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::BashOutputChunk(
            xai_grok_tools::notification::types::BashOutputChunk {
                base: xai_grok_tools::notification::types::BashNotificationBase {
                    tool_call_id: "call-1".into(),
                    command: "echo hi".into(),
                    output: b"hi\n".to_vec(),
                    total_bytes: 3,
                    truncated: false,
                    cwd: PathBuf::from("/tmp"),
                },
            },
        );
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        let persisted_id = match persistence_rx.try_recv().expect("chunk must be persisted") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => notif
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("persisted ACP bridge lines must carry an eventId")
                .to_string(),
            other => panic!("expected PersistenceMsg::Update(Acp(..)), got {other:?}"),
        };
        let broadcast_id = match gateway_rx.try_recv().expect("chunk must be broadcast") {
            xai_acp_lib::AcpClientMessage::SessionNotification(args) => args
                .request
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("broadcast must carry the eventId")
                .to_string(),
            other => panic!("expected SessionNotification, got {other:?}"),
        };
        assert_eq!(persisted_id, broadcast_id);
    }
    #[tokio::test]
    async fn scheduled_task_removed_is_persisted() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let removed = xai_grok_tools::notification::ScheduledTaskRemoved {
            task_id: "loop-1".into(),
            generation: "generation-a".into(),
            revision: 2,
        };
        handle_scheduled_task_removed(&config, removed, None)
            .await
            .unwrap();
        let msg = persistence_rx
            .try_recv()
            .expect("scheduled_task_removed must be persisted");
        match msg {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(matches!(
                    &notif.update,
                    crate::extensions::notification::SessionUpdate::ScheduledTaskDeleted { .. }
                ));
                assert!(
                    xai_persisted_event_id(&notif).is_some(),
                    "the persisted deletion line must be stamped"
                );
                let meta = notif.meta.as_ref().expect("scheduler metadata");
                assert_eq!(meta["x.ai/schedulerGeneration"], "generation-a");
                assert_eq!(meta["x.ai/schedulerRevision"], 2);
            }
            _ => panic!("expected PersistenceMsg::Update(Xai(ScheduledTaskDeleted))"),
        }
    }
    #[tokio::test]
    async fn acknowledged_scheduler_removal_appends_before_ack_and_broadcast() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let removed = xai_grok_tools::notification::ScheduledTaskRemoved {
            task_id: "loop-ack".into(),
            generation: "generation-a".into(),
            revision: 17,
        };
        let (acknowledgement, mut receipt) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let persistence = async {
            let PersistenceMsg::AppendUpdateDurablyAndAck {
                update: crate::session::storage::SessionUpdate::Xai(notification),
                respond_to,
            } = persistence_rx.recv().await.expect("durable append")
            else {
                panic!("expected durable scheduler tombstone");
            };
            assert_eq!(notification.meta.unwrap()["x.ai/schedulerRevision"], 17);
            assert!(gateway_rx.try_recv().is_err());
            assert!(matches!(
                receipt.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            respond_to.send(Ok(())).unwrap();
            receipt.await.unwrap().unwrap();
        };
        let (result, ()) = tokio::join!(
            handle_scheduled_task_removed(&config, removed, Some(acknowledgement)),
            persistence,
        );
        result.unwrap();
        assert!(matches!(
            gateway_rx.try_recv(),
            Ok(xai_acp_lib::AcpClientMessage::ExtNotification(_))
        ));
    }
    fn xai_persisted_event_id(
        notif: &crate::extensions::notification::SessionNotification,
    ) -> Option<String> {
        notif
            .meta
            .as_ref()
            .and_then(|m| m.get("eventId"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
    /// Per-site stamp pins for the bridge emitters not covered by the
    /// representative chokepoint tests: deleting any one `stamp_event_id`
    /// call must fail a test (an id-less persisted line silently disables
    /// incremental reconnect for the session).
    #[tokio::test]
    async fn task_backgrounded_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::BashExecutionBackgrounded(
            xai_grok_tools::notification::types::BashExecutionBackgrounded {
                base: xai_grok_tools::notification::types::BashNotificationBase {
                    tool_call_id: "call-bg".into(),
                    command: "sleep 100".into(),
                    output: Vec::new(),
                    total_bytes: 0,
                    truncated: false,
                    cwd: PathBuf::from("/tmp"),
                },
                output_file: PathBuf::from("/tmp/out.log"),
                task_id: "task-bg".into(),
                monitor_description: None,
                description: None,
            },
        );
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(xai_persisted_event_id(&notif).is_some());
            }
            _ => panic!("expected Xai update"),
        }
    }
    #[tokio::test]
    async fn task_completed_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let snapshot = make_task_snapshot("mon-1", TaskKind::Monitor);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(xai_persisted_event_id(&notif).is_some());
            }
            _ => panic!("expected Xai update"),
        }
    }
    #[tokio::test]
    async fn current_mode_update_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        emit_current_mode_update(&config, xai_grok_tools::types::SessionMode::Plan).await;
        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => {
                assert!(matches!(
                    notif.update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .is_some(),
                    "the persisted mode line must be stamped"
                );
            }
            _ => panic!("expected Acp update"),
        }
    }
    #[test]
    fn durable_append_mapping_respects_commit_disposition() {
        assert!(
            durable_append_landed(Err(DurableAppendError::Committed(std::io::Error::other(
                "summary failed"
            ),)))
            .is_ok()
        );
        for failure in [
            DurableAppendError::NotCommitted(std::io::Error::other("append failed")),
            DurableAppendError::AcknowledgementLost(std::io::Error::other("lost")),
        ] {
            assert!(durable_append_landed(Err(failure)).is_err());
        }
    }
    #[tokio::test]
    async fn scheduled_task_fired_is_not_persisted() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::ScheduledTaskFired(
            xai_grok_tools::notification::types::ScheduledTaskFired {
                task_id: "loop-1".into(),
                prompt: "check deploy".into(),
                human_schedule: "every 5 minutes".into(),
                next_fire_at: Some("2026-01-01T00:00:00Z".into()),
                subagent_id: Some("subagent-1".into()),
                generation: "generation-a".into(),
                revision: 3,
            },
        );
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        assert!(
            persistence_rx.try_recv().is_err(),
            "scheduled_task_fired must NOT be persisted (recurring \u{2192} unbounded log growth)"
        );
        let fired = gateway_rx
            .try_recv()
            .expect("scheduled fire must be broadcast");
        let xai_acp_lib::AcpClientMessage::ExtNotification(fired) = fired else {
            panic!("expected scheduler fire notification");
        };
        let value: serde_json::Value = serde_json::from_str(fired.request.params.get()).unwrap();
        assert_eq!(value["_meta"]["x.ai/schedulerGeneration"], "generation-a");
        assert_eq!(value["_meta"]["x.ai/schedulerRevision"], 3);
    }
    fn make_monitor_event_notification(task_id: &str, owner: Option<&str>) -> ToolNotification {
        ToolNotification::MonitorEvent(xai_grok_tools::notification::types::MonitorEvent {
            task_id: task_id.into(),
            description: "errors in deploy.log".into(),
            event_text: format!("<monitor-event task_id=\"{task_id}\">boom</monitor-event>"),
            raw_text: "boom".into(),
            owner_session_id: owner.map(str::to_string),
        })
    }
    #[tokio::test]
    async fn cross_session_monitor_event_is_dropped() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let notification = make_monitor_event_notification("mon-foreign", Some("other-session"));
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        assert!(
            cmd_rx.try_recv().is_err(),
            "cross-session monitor event must not be injected into this session"
        );
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
                assert_ne!(
                    args.request.method.as_ref(),
                    "x.ai/monitor_event",
                    "cross-session monitor event must not be forwarded to the pager"
                );
            }
        }
    }
    #[tokio::test]
    async fn same_session_monitor_event_is_injected() {
        let (config, mut cmd_rx) = make_test_config();
        let notification = make_monitor_event_notification("mon-own", Some("test-session"));
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        match cmd_rx
            .try_recv()
            .expect("own-session monitor event must be injected")
        {
            SessionCommand::InjectNotification { source, .. } => match source {
                NotificationSource::MonitorEvent { task_id } => {
                    assert_eq!(task_id, "mon-own")
                }
                _ => panic!("expected MonitorEvent notification source"),
            },
            _ => panic!("expected InjectNotification"),
        }
    }
    #[tokio::test]
    async fn legacy_monitor_event_without_owner_is_injected() {
        let (config, mut cmd_rx) = make_test_config();
        let notification = make_monitor_event_notification("mon-legacy", None);
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        assert!(
            matches!(
                cmd_rx
                    .try_recv()
                    .expect("legacy (no-owner) monitor event must be injected"),
                SessionCommand::InjectNotification {
                    source: NotificationSource::MonitorEvent { .. },
                    ..
                }
            ),
            "legacy monitor event should be injected as a MonitorEvent notification"
        );
    }
    #[tokio::test]
    async fn block_waited_task_skips_auto_wake_prompt() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let mut snapshot = make_task_snapshot("bg-waited", TaskKind::Bash);
        snapshot.block_waited = true;
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "block_waited completion should not send Prompt or InjectNotification"
        );
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }
    #[tokio::test]
    async fn explicitly_killed_task_skips_auto_wake_prompt() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let mut snapshot = make_task_snapshot("bg-killed", TaskKind::Bash);
        snapshot.explicitly_killed = true;
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => {
                assert_eq!(notification_type, "task_complete")
            }
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "explicitly_killed completion should not send Prompt or InjectNotification"
        );
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }
    #[tokio::test]
    async fn bash_task_completed_falls_back_when_auto_wake_disabled() {
        let (mut config, mut cmd_rx) = make_test_config();
        config.auto_wake_enabled = false;
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let snapshot = make_task_snapshot("bg-disabled", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        let cmd = cmd_rx.try_recv().expect("expected InjectNotification");
        match cmd {
            SessionCommand::InjectNotification {
                prompt_id,
                prompt_blocks,
                priority,
                source,
                ..
            } => {
                assert!(prompt_id.starts_with("bash-completed-"));
                assert_eq!(priority, NotificationPriority::Later);
                assert!(
                    matches!(source, NotificationSource::BashTaskCompleted { ref task_id
                    } if task_id == "bg-disabled")
                );
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    _ => panic!("expected text block"),
                };
                assert!(text.contains(r#"get_command_or_subagent_output("bg-disabled")"#));
                assert!(!text.contains(r#"get_task_output("bg-disabled")"#));
                assert!(!text.contains("response:"));
            }
            _ => panic!("expected InjectNotification"),
        }
        let hook_cmd = cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete");
        match hook_cmd {
            SessionCommand::DispatchNotificationHook {
                notification_type,
                message,
                ..
            } => {
                assert_eq!(notification_type, "task_complete");
                assert_eq!(
                    message.as_deref(),
                    Some("Background task completed: bg-disabled")
                );
            }
            _ => panic!("expected DispatchNotificationHook"),
        }
    }
    #[tokio::test]
    async fn bash_completion_uses_single_task_id_clone() {
        let (config, mut cmd_rx) = make_test_config();
        let snapshot = make_task_snapshot("unique-id-789", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();
        handle_notification_with_admission(&config, notification, &mut offsets, &mut cmd_rx, true)
            .await;
        let cmd = cmd_rx.try_recv().unwrap();
        if let SessionCommand::Prompt { prompt_id, .. } = cmd {
            assert_eq!(prompt_id, "task-completed-unique-id-789");
        } else {
            panic!("expected Prompt");
        }
    }
    fn extract_current_mode_id(notification: &acp::SessionNotification) -> Option<&str> {
        match &notification.update {
            acp::SessionUpdate::CurrentModeUpdate(cmu) => Some(cmu.current_mode_id.0.as_ref()),
            _ => None,
        }
    }
    /// Regression: `PlanModeExited` must emit `CurrentModeUpdate("default")`
    /// onto both the gateway and the persistence stream. Without this,
    /// agent-driven plan approvals leave the TUI stuck in plan mode.
    #[tokio::test]
    async fn plan_mode_exited_emits_current_mode_update_default() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }
        *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
        *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-1".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        let mut gateway_modes = Vec::new();
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg
                && let Some(id) = extract_current_mode_id(&args.request)
            {
                gateway_modes.push(id.to_string());
            }
        }
        assert_eq!(
            gateway_modes,
            vec!["default".to_string()],
            "PlanModeExited should emit exactly one CurrentModeUpdate(default) to the gateway"
        );
        let mut persisted_modes = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) = msg
                && let Some(id) = extract_current_mode_id(&notif)
            {
                persisted_modes.push(id.to_string());
            }
        }
        assert_eq!(
            persisted_modes,
            vec!["default".to_string()],
            "PlanModeExited should persist exactly one CurrentModeUpdate(default)"
        );
        assert!(matches!(
            *config.current_prompt_mode.lock(),
            crate::session::plan_mode::PromptMode::Agent
        ));
    }
    /// Default (grok) polarity: the exit_plan_mode tool result is the model's
    /// only exit signal, so an approved `PlanModeExited` must NOT arm the
    /// deferred exit reminder — in memory or in the persisted snapshot.
    /// Sibling of `plan_mode_exited_arms_exit_reminder_when_gated`.
    #[tokio::test]
    async fn plan_mode_exited_does_not_arm_exit_reminder_by_default() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }
        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-grok".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        assert!(
            !config.plan_mode.lock().has_pending_exit_reminder(),
            "approved exit must not arm the deferred exit reminder"
        );
        let mut persisted_plan_snapshots = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::PlanModeState(snapshot) = msg {
                persisted_plan_snapshots.push(snapshot);
            }
        }
        assert!(
            !persisted_plan_snapshots.is_empty()
                && persisted_plan_snapshots
                    .iter()
                    .all(|s| !s.pending_exit_reminder),
            "persisted plan-mode snapshot must not carry the exit reminder"
        );
    }
    /// Gated counterpart: when `queue_exit_reminder_on_approved_exit` is
    /// set, an approved `PlanModeExited` must arm the next-turn exit
    /// reminder and persist it.
    #[tokio::test]
    async fn plan_mode_exited_arms_exit_reminder_when_gated() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        config
            .queue_exit_reminder_on_approved_exit
            .store(true, std::sync::atomic::Ordering::Relaxed);
        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }
        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-gated".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        assert!(
            config.plan_mode.lock().has_pending_exit_reminder(),
            "gated approved exit must arm the next-turn exit reminder"
        );
        let mut persisted_plan_snapshots = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::PlanModeState(snapshot) = msg {
                persisted_plan_snapshots.push(snapshot);
            }
        }
        assert!(
            !persisted_plan_snapshots.is_empty()
                && persisted_plan_snapshots
                    .iter()
                    .all(|s| s.pending_exit_reminder),
            "persisted plan-mode snapshot must carry the armed exit reminder"
        );
    }
    /// Symmetric to the exit test: `PlanModeEntered` emits
    /// `CurrentModeUpdate("plan")`.
    #[tokio::test]
    async fn plan_mode_entered_emits_current_mode_update_plan() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::PlanModeEntered(
            xai_grok_tools::notification::types::PlanModeEntered {
                tool_call_id: "tc-enter-1".into(),
            },
        );
        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;
        let mut gateway_modes = Vec::new();
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg
                && let Some(id) = extract_current_mode_id(&args.request)
            {
                gateway_modes.push(id.to_string());
            }
        }
        assert_eq!(gateway_modes, vec!["plan".to_string()]);
        let mut persisted_modes = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) = msg
                && let Some(id) = extract_current_mode_id(&notif)
            {
                persisted_modes.push(id.to_string());
            }
        }
        assert_eq!(persisted_modes, vec!["plan".to_string()]);
    }
    /// Build a completed-bash `TaskSnapshot` whose `output` is large enough
    /// to trip the inline-completion truncation cap, with a concrete
    /// `output_file` path so the disk-pointer footer is exercised end-to-end.
    fn make_large_bash_snapshot(task_id: &str, output_file: PathBuf) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.into(),
            command: "yes hello | head -c 20000".into(),
            display_command: None,
            cwd: String::new(),
            start_time: std::time::SystemTime::now(),
            end_time: Some(std::time::SystemTime::now()),
            output: "h".repeat(20_000),
            output_file,
            truncated: true,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind: TaskKind::Bash,
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: None,
        }
    }
    /// Extract the auto-wake prompt text emitted on the session command channel.
    fn auto_wake_prompt_text(cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>) -> String {
        let cmd = cmd_rx.try_recv().expect("expected Prompt");
        match cmd {
            SessionCommand::Prompt { prompt_blocks, .. } => match &prompt_blocks[0] {
                acp::ContentBlock::Text(t) => t.text.clone(),
                _ => panic!("expected text block"),
            },
            _ => panic!("expected Prompt"),
        }
    }
    /// Extract the InjectNotification prompt text emitted on the session
    /// command channel (auto-wake-disabled fallback path).
    fn inject_notification_prompt_text(
        cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>,
    ) -> String {
        let cmd = cmd_rx.try_recv().expect("expected InjectNotification");
        match cmd {
            SessionCommand::InjectNotification { prompt_blocks, .. } => match &prompt_blocks[0] {
                acp::ContentBlock::Text(t) => t.text.clone(),
                _ => panic!("expected text block"),
            },
            _ => panic!("expected InjectNotification"),
        }
    }
    /// Bash completion with a large output and no polling tool (compat-harness
    /// toolset) renders the truncation marker AND the disk-pointer footer
    /// pointing the model at `output_file` via the resolved Read tool name.
    /// Covers BOTH the auto-wake branch and the auto-wake-disabled fallback
    /// so the truncation + footer behaviour stays consistent across both
    /// completion-injection paths.
    #[tokio::test]
    async fn bash_completion_renders_disk_pointer_footer_in_both_branches() {
        let output_file = PathBuf::from("/tmp/bg-disk-pointer.log");
        let (config_auto, mut cmd_rx_auto) = make_test_config();
        config_auto
            .read_tool_name
            .set(Some("read_file".to_string()))
            .expect("fresh slot");
        let snapshot = make_large_bash_snapshot("bg-disk-1", output_file.clone());
        let mut offsets = HashMap::new();
        handle_notification_with_admission(
            &config_auto,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
            &mut cmd_rx_auto,
            true,
        )
        .await;
        let prompt = auto_wake_prompt_text(&mut cmd_rx_auto);
        assert!(
            prompt.contains("[Output truncated"),
            "auto-wake: expected truncation marker, got: {prompt}"
        );
        let expected_footer = format!(
            "Use read_file on {} for full content",
            output_file.display()
        );
        assert!(
            prompt.contains(&expected_footer),
            "auto-wake: expected disk-pointer footer `{expected_footer}`, got: {prompt}"
        );
        assert!(
            prompt.contains("bg-disk-1"),
            "auto-wake: prompt must reference task id"
        );
        let (mut config_no_wake, mut cmd_rx_no_wake) = make_test_config();
        config_no_wake.auto_wake_enabled = false;
        config_no_wake
            .read_tool_name
            .set(Some("read_file".to_string()))
            .expect("fresh slot");
        let snapshot = make_large_bash_snapshot("bg-disk-2", output_file.clone());
        let mut offsets = HashMap::new();
        handle_notification(
            &config_no_wake,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        let prompt = inject_notification_prompt_text(&mut cmd_rx_no_wake);
        assert!(
            prompt.contains("[Output truncated"),
            "auto-wake-disabled: expected truncation marker, got: {prompt}"
        );
        let expected_footer = format!(
            "Use read_file on {} for full content",
            output_file.display()
        );
        assert!(
            prompt.contains(&expected_footer),
            "auto-wake-disabled: expected disk-pointer footer `{expected_footer}`, got: {prompt}"
        );
        assert!(
            prompt.contains("bg-disk-2"),
            "auto-wake-disabled: prompt must reference task id"
        );
    }
}
