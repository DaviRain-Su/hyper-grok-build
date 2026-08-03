//! Forwards recorded session updates back to a loading client, fitting
//! completion records written before the size limit existed.

use agent_client_protocol as acp;

use super::{MvpAgent, mark_as_replay, stamp_meta_value};

impl MvpAgent {
    /// Records written before completions were bounded can still be too long
    /// for a client to read. `None` drops one that cannot be shrunk, which
    /// costs a completion event but keeps the connection.
    fn fitted_replay_params(
        params: Box<serde_json::value::RawValue>,
    ) -> Option<Box<serde_json::value::RawValue>> {
        use crate::tools::task_completed_frame::{Refit, refit_recorded};

        match refit_recorded(&params) {
            Refit::Unchanged => Some(params),
            Refit::Fitted(fitted) => Some(fitted.into_inner()),
            Refit::Unfittable => {
                tracing::warn!(
                    bytes = params.get().len(),
                    "replay: dropping a completion too long to send"
                );
                None
            }
        }
    }

    /// Forward one raw JSONL replay line and collect its completion receiver.
    ///
    /// Dispatches by on-disk method name:
    /// - ACP updates (`"session/update"`) → typed `SessionNotification` for correct
    ///   TUI dispatch (direct dispatch preserves Rust types, not method strings).
    /// - xAI updates (`"_x.ai/session/update"`) → `ExtNotification`.
    ///
    /// When `mark_replay` is true, the notification is tagged with
    /// `_meta.isReplay: true` so the client knows it's historical data.
    /// Cursor-based reconnects set this to false for events after the cursor
    /// so the client processes them as live updates.
    pub(super) fn forward_raw_replay_line(
        &self,
        line: &str,
        persist_data: Option<&serde_json::Value>,
        target_client_id: Option<&serde_json::Value>,
        completions: &mut Vec<tokio::sync::oneshot::Receiver<xai_acp_lib::AcpResult<()>>>,
        mark_replay: bool,
        pending_tool_calls: &mut std::collections::HashMap<acp::ToolCallId, acp::ToolCall>,
    ) {
        use crate::session::storage::RawLinePeek;

        let env = match serde_json::from_str::<RawLinePeek<'_>>(line) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(?e, "replay: skipping unparseable JSONL line");
                return;
            }
        };
        // updates.jsonl only persists `_x.ai/session/update` and `session/update`.
        // Unknown methods fall through to the ACP parse below and are dropped on error.
        let method = env.method.unwrap_or("session/update");
        let Some(raw_params) = env.params else {
            tracing::debug!("replay: skipping JSONL line with no params");
            return;
        };
        let is_xai = method == "_x.ai/session/update";

        if is_xai {
            // The fast-path forwards raw params with no `_meta` round-trip, so it
            // can stamp nothing. When a `target_client_id` is present we MUST take
            // the injection path instead, otherwise the replay would lose the
            // target and the leader would broadcast it to every subscriber.
            if target_client_id.is_none() && !mark_replay {
                // Fast-path: forward raw params without Value round-trip.
                if let Ok(owned) =
                    serde_json::value::RawValue::from_string(raw_params.get().to_owned())
                    && let Some(owned) = Self::fitted_replay_params(owned)
                {
                    completions.push(self.gateway.forward_with_completion(
                        acp::ExtNotification::new(
                            "x.ai/session/update",
                            std::sync::Arc::from(owned),
                        ),
                    ));
                }
            } else {
                // Inject _meta — requires parse + re-serialize.
                let Ok(mut params) = serde_json::from_str::<serde_json::Value>(raw_params.get())
                else {
                    tracing::debug!("replay: skipping xAI update with unparseable params");
                    return;
                };
                if let Some(obj) = params.as_object_mut() {
                    let meta = obj.entry("_meta").or_insert_with(|| serde_json::json!({}));
                    if let Some(m) = meta.as_object_mut() {
                        // `isReplay` only applies to historical replay events, not the
                        // post-cursor live deltas that reach this path when a target is set.
                        if mark_replay {
                            m.insert("isReplay".to_string(), serde_json::json!(true));
                        }
                        if let Some(pd) = persist_data {
                            m.insert("x.ai/persist".to_string(), pd.clone());
                        }
                        if let Some(tid) = target_client_id {
                            m.insert("x.ai/leaderClientId".to_string(), tid.clone());
                        }
                    }
                }
                // Fit after `_meta` is added, so what is measured is what is sent.
                if let Ok(raw_val) = serde_json::value::to_raw_value(&params)
                    && let Some(raw_val) = Self::fitted_replay_params(raw_val)
                {
                    completions.push(self.gateway.forward_with_completion(
                        acp::ExtNotification::new(
                            "x.ai/session/update",
                            std::sync::Arc::from(raw_val),
                        ),
                    ));
                }
            }
        } else {
            // ACP — forward as typed SessionNotification for correct TUI dispatch.
            let Ok(mut notification) =
                serde_json::from_str::<acp::SessionNotification>(raw_params.get())
            else {
                tracing::debug!("replay: skipping ACP update with unparseable params");
                return;
            };
            // Collapse ToolCall + all ToolCallUpdates into a single
            // pre-completed ToolCall during replay. This gives the pager
            // one push() per tool call instead of 2-4.
            //
            // - ToolCall (registration): buffer by ID, don't forward yet.
            // - ToolCallUpdate status=None (start metadata): merge into buffer.
            // - ToolCallUpdate InProgress/Pending (streaming): drop.
            // - ToolCallUpdate Completed/Failed: merge into buffer, forward
            //   as a single SessionUpdate::ToolCall with final status.
            match &mut notification.update {
                acp::SessionUpdate::ToolCall(tc) => {
                    let is_pre_completed = matches!(
                        tc.status,
                        acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                    );
                    if is_pre_completed {
                        // Already complete — forward as-is (no updates will follow).
                    } else {
                        pending_tool_calls.insert(tc.tool_call_id.clone(), tc.clone());
                        return;
                    }
                }
                acp::SessionUpdate::ToolCallUpdate(u) => {
                    match u.fields.status {
                        Some(acp::ToolCallStatus::Completed)
                        | Some(acp::ToolCallStatus::Failed) => {
                            if let Some(mut base) = pending_tool_calls.remove(&u.tool_call_id) {
                                base.update(std::mem::take(&mut u.fields));
                                notification.update = acp::SessionUpdate::ToolCall(base);
                            }
                            // If no buffered base, forward the ToolCallUpdate as-is.
                        }
                        None => {
                            // Start metadata (title, kind, rawInput, locations).
                            // Merge into the buffered ToolCall.
                            if let Some(base) = pending_tool_calls.get_mut(&u.tool_call_id) {
                                base.update(std::mem::take(&mut u.fields));
                            }
                            return;
                        }
                        _ => return, // InProgress / Pending — drop
                    }
                }
                _ => {}
            }
            if mark_replay {
                mark_as_replay(&mut notification.meta, persist_data);
            }
            // Stamp the leader unicast target regardless of mark_replay so the
            // leader routes both historical and post-cursor live deltas only to
            // the loading client.
            if let Some(tid) = target_client_id {
                stamp_meta_value(&mut notification.meta, "x.ai/leaderClientId", tid);
            }
            completions.push(self.gateway.forward_with_completion(notification));
        }
    }
}
