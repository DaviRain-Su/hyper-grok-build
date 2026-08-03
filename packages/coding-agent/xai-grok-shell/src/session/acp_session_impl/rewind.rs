//! Rewind concern for `SessionActor`: rewind points, cross-compaction
//! replay detection, and `handle_rewind`.

use super::*;

impl SessionActor {
    pub(super) async fn close_rewind_window(&self) {
        let mut state = self.state.lock().await;
        state.rewindable = false;
    }

    /// Returns the `prompt_index → num_file_snapshots` map from the on-disk
    /// snapshot index (independent of the chat-state prompt index). The bridge
    /// joins these onto the server's rewind points.
    pub(super) async fn rewind_file_counts(&self) -> std::collections::HashMap<usize, usize> {
        self.file_state_tracker
            .get_rewind_point_metas()
            .await
            .into_iter()
            .map(|m| (m.prompt_index, m.num_file_snapshots))
            .collect()
    }

    /// Get available rewind points for this session.
    ///
    /// Every prompt is a checkpoint — the list always contains `[0, 1, ..., N-1]`
    /// where N is the current prompt_index. File snapshots may or may not exist
    /// for each checkpoint (indicated by `has_file_changes`).
    pub(super) async fn get_rewind_points(&self) -> RewindPointsResponse {
        // Metadata only — don't materialize the (huge) file-content snapshots
        // just to render the picker.
        let file_metas = self.file_state_tracker.get_rewind_point_metas().await;

        // Query prompt state from the chat state actor.
        let snapshot = self.chat_state_handle.snapshot().await;
        let (prompts, current_prompt_index) = match snapshot {
            Some(ref s) => (s.prompt_texts.clone(), s.prompt_index),
            None => (vec![], 0),
        };

        // Build a lookup of which prompt indices have file snapshots.
        let file_meta_map: std::collections::HashMap<
            usize,
            &xai_grok_workspace::session::file_state::RewindPointMeta,
        > = file_metas.iter().map(|m| (m.prompt_index, m)).collect();

        // Generate a rewind point for every prompt 0..current_prompt_index.
        let rewind_points = (0..current_prompt_index)
            .map(|idx| {
                let prompt_preview = prompts.get(idx).and_then(|text| {
                    let clean_text = extract_user_query(text);
                    let first_line = clean_text
                        .lines()
                        .map(|l| l.trim())
                        .find(|l| !l.is_empty())
                        .unwrap_or("");

                    if first_line.is_empty() {
                        None
                    } else if first_line.chars().count() > 60 {
                        Some(format!("{}...", crate::util::truncate(first_line, 57)))
                    } else {
                        Some(first_line.to_string())
                    }
                });

                let file_meta = file_meta_map.get(&idx);
                let num_file_snapshots = file_meta.map_or(0, |m| m.num_file_snapshots);
                let created_at = file_meta
                    .map(|m| m.created_at.to_rfc3339())
                    .unwrap_or_default();

                RewindPointInfo {
                    prompt_index: idx,
                    created_at,
                    num_file_snapshots,
                    has_file_changes: num_file_snapshots > 0,
                    prompt_preview,
                }
            })
            .collect();

        RewindPointsResponse { rewind_points }
    }

    /// Load user prompts from `updates.jsonl` in chronological order.
    ///
    /// Each `UserMessageChunk` sequence is merged into a single prompt string.
    /// `RewindMarker` entries truncate the list back to the marker's target so
    /// only prompts from the current timeline are returned.
    ///
    /// Uses [`PromptExtractIterator`] which peeks at the `update.sessionUpdate`
    /// discriminant field without fully deserialising every notification.  This
    /// avoids large `acp::SessionNotification` allocations for the many update
    /// types (tool calls, assistant chunks, etc.) that are irrelevant to prompt
    /// extraction.
    pub(super) fn load_user_prompts_from_updates(
        updates_path: &std::path::Path,
    ) -> std::io::Result<Vec<String>> {
        use crate::session::storage::{PromptExtractIterator, collect_prompts_from_events};

        let Some(iter) = PromptExtractIterator::open(updates_path)? else {
            return Ok(vec![]);
        };

        tracing::debug!(
            path = %updates_path.display(),
            "load_user_prompts_from_updates: starting selective scan"
        );

        let prompts = collect_prompts_from_events(iter);

        tracing::debug!(
            prompt_count = prompts.len(),
            "load_user_prompts_from_updates: done"
        );

        Ok(prompts)
    }

    /// Check whether rewinding to `target_index` needs replay because a
    /// compaction has occurred, meaning we need to replay `updates.jsonl`
    /// to reconstruct the conversation.
    ///
    /// Always use replay when compaction has occurred, regardless of whether
    /// the target is before, at, or after the compaction point.
    ///
    /// Post-compaction, the in-memory conversation has a different number of
    /// User messages than `prompt_index` implies (compaction collapses N+1
    /// user messages into ~3). `truncate_to_prompt_index` counts User items
    /// to find the cut point, so it produces wrong results for ALL
    /// post-compaction targets — not just at the boundary.
    ///
    /// `replay_to_prompt` reads `updates.jsonl` from scratch and handles
    /// compaction checkpoints correctly, so it always produces the right
    /// conversation regardless of target position.
    async fn needs_compaction_replay(&self) -> bool {
        let last = self
            .chat_state_handle
            .snapshot()
            .await
            .and_then(|s| s.last_compaction_prompt_index);
        match last {
            Some(compaction_at) => {
                tracing::info!(
                    compaction_at,
                    "Compaction detected — using replay for rewind"
                );
                true
            }
            None => false,
        }
    }

    /// Handle a rewind request with mode support.
    ///
    /// Semantics: "restore state before prompt N ran" — prompts 0..N-1 are kept.
    ///
    /// Modes:
    /// - `All`: roll back both conversation and files (full time-travel)
    /// - `ConversationOnly`: roll back conversation, leave files untouched
    /// - `FilesOnly`: roll back files, leave conversation untouched
    pub(super) async fn handle_rewind(
        &self,
        request: RewindRequest,
    ) -> anyhow::Result<RewindResponse> {
        // Track revert for feedback signals
        self.signals_handle().mark_reverted();

        let target_index = request.target_prompt_index;
        let mode = request.mode;

        // Validate: target must be less than current prompt_index. FilesOnly
        // reverts the on-disk snapshot index (bounded by `get_rewind_points`,
        // not the conversation), so it is exempt — the chat-state prompt index
        // is empty in bridge mode, where the conversation lives server-side.
        let current_prompt_index = self.chat_state_handle.get_prompt_index().await;
        if mode != RewindMode::FilesOnly && target_index >= current_prompt_index {
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: target_index,
                mode,
                reverted_files: vec![],
                clean_files: vec![],
                conflicts: vec![],
                prompt_text: None,
                error: Some(format!(
                    "Cannot rewind to prompt #{} — current prompt index is {}. \
                     Valid targets: 0..{}",
                    target_index,
                    current_prompt_index,
                    current_prompt_index.saturating_sub(1)
                )),
            });
        }

        // ── Build file revert preview (for All and FilesOnly modes) ─────
        let mut clean_files = Vec::new();
        let mut conflicts = Vec::new();

        let wants_file_revert = matches!(mode, RewindMode::All | RewindMode::FilesOnly);
        let wants_conversation_rewind =
            matches!(mode, RewindMode::All | RewindMode::ConversationOnly);

        // Collect files that would be reverted and detect conflicts.
        // This is read-only — no mutations happen here.
        let mut files_to_revert: std::collections::HashMap<
            xai_grok_workspace::session::file_state::FlexiblePath,
            Option<String>,
        > = std::collections::HashMap::new();

        if wants_file_revert {
            let all_points = self.file_state_tracker.get_rewind_points().await;

            for point in all_points.iter().filter(|p| p.prompt_index >= target_index) {
                for (path, before_snapshot) in &point.file_snapshots {
                    // Only keep the earliest snapshot for each file
                    files_to_revert
                        .entry(path.clone())
                        .or_insert_with(|| before_snapshot.content.clone());
                }
            }

            // Build conflict/clean lists for the preview
            for path in files_to_revert.keys() {
                let current_content = self
                    .tool_context
                    .fs
                    .try_read_to_string(path)
                    .await
                    .unwrap_or(None);

                // Find the latest after_snapshot for this file (what the agent
                // most recently left it as) for conflict detection.
                let after_content = all_points
                    .iter()
                    .rev()
                    .find_map(|p| p.after_snapshots.get(path))
                    .and_then(|s| s.content.clone());

                let is_clean = current_content == after_content;

                if is_clean {
                    clean_files.push(path.to_string());
                } else {
                    let conflict_type = if current_content.is_none() && after_content.is_some() {
                        "deleted_externally"
                    } else if current_content.is_some() && after_content.is_none() {
                        "created_externally"
                    } else {
                        "modified_externally"
                    };
                    conflicts.push(RewindConflictInfo {
                        path: path.to_string(),
                        conflict_type: conflict_type.to_string(),
                    });
                }
            }
        }

        // ── Preview mode (force=false): pure dry run, no mutations ────
        // Return what WOULD happen so the TUI can show a confirmation
        // modal. Nothing is written, deleted, or truncated.
        if !request.force {
            let error = if !conflicts.is_empty() {
                Some("External modifications detected. Confirm to revert anyway.".to_string())
            } else {
                None
            };
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: target_index,
                mode,
                reverted_files: vec![],
                clean_files,
                conflicts,
                prompt_text: None,
                error,
            });
        }

        // ── Commit mode (force=true): execute the rewind ─────────────

        // Execute file revert
        let mut reverted_files = Vec::new();
        if wants_file_revert {
            for (rel_path, content) in files_to_revert {
                match &content {
                    Some(data) => {
                        if let Err(e) = self
                            .tool_context
                            .fs
                            .write_file(&rel_path, data.as_bytes())
                            .await
                        {
                            tracing::warn!(?e, "Failed to restore file during rewind");
                            continue;
                        }
                    }
                    None => {
                        if self
                            .tool_context
                            .fs
                            .exists(&rel_path)
                            .await
                            .unwrap_or(false)
                            && let Err(e) = self.tool_context.fs.delete_file(&rel_path).await
                        {
                            tracing::warn!(?e, "Failed to delete file during rewind");
                        }
                    }
                }
                reverted_files.push(rel_path.to_string());
            }
        }

        // Execute conversation rewind
        let mut prompt_text: Option<String> = None;
        if wants_conversation_rewind {
            let session_dir = crate::session::persistence::session_dir(&self.session_info);
            let updates_path = session_dir.join("updates.jsonl");

            if let Some(snap) = self.chat_state_handle.snapshot().await {
                prompt_text = snap.prompt_texts.get(target_index).cloned();
            }

            // Store for edit-and-retry detection in the next prompt() call
            if let Ok(mut pending) = self.rewind_pending_prompt.lock() {
                *pending = prompt_text.clone();
            }

            // Check cross-compaction before rewinding.
            let needs_replay = self.needs_compaction_replay().await;

            // Get conversation from the chat state actor for truncation logic.
            let mut conversation = self.chat_state_handle.get_conversation().await;

            // Cross-compaction replay recomputes whether a compaction summary
            // survives; `None` keeps the existing marker (standard truncation).
            let mut replay_compaction_marker: Option<Option<usize>> = None;

            if needs_replay {
                // Cross-compaction rewind: reconstruct conversation from updates.jsonl.
                // Run on the blocking pool since replay does synchronous file I/O
                // (reading checkpoint files + scanning updates.jsonl).
                let replay_updates = updates_path.clone();
                let replay_session_dir = session_dir.clone();
                let replay_target = target_index;
                let replay_result = tokio::task::spawn_blocking(move || {
                    crate::session::helpers::replay::replay_to_prompt(
                        &replay_updates,
                        &replay_session_dir,
                        replay_target,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {e}"))?;
                match replay_result {
                    Ok(replay_result) => {
                        tracing::info!(
                            target_index,
                            prompt_index_reached = replay_result.prompt_index_reached,
                            conversation_len = replay_result.conversation.len(),
                            "Cross-compaction rewind: conversation reconstructed via replay"
                        );
                        // The rebuilt conversation drops the summary unless a
                        // checkpoint survived; carry the recomputed marker to
                        // the snapshot restore so the stale value isn't reused.
                        replay_compaction_marker = Some(replay_result.last_compaction_prompt_index);
                        // The replay result may or may not include the session
                        // preamble (System + User(user_info)):
                        // - Checkpoint loaded (target >= compaction_at): the
                        //   checkpoint's compacted_history already has System +
                        //   User prefix. Use directly.
                        // - Raw updates (target < compaction_at): replay only
                        //   accumulates user/agent turns from updates.jsonl.
                        //   Prepend System + original User(user_info) so the
                        //   model sees the same preamble it originally saw.
                        if matches!(
                            replay_result.conversation.first(),
                            Some(ConversationItem::System(_))
                        ) {
                            conversation = replay_result.conversation;
                        } else {
                            // Keep System (index 0). Replace User(user_info) at
                            // index 1 with the original from the checkpoint if
                            // available, otherwise keep the current one.
                            if let Some(ui0) = replay_result.original_user_info {
                                conversation.truncate(1); // keep System only
                                conversation.push(ConversationItem::user(ui0));
                            } else {
                                conversation.truncate(2); // keep System + current user_info
                            }
                            conversation.extend(replay_result.conversation);
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            ?e,
                            target_index,
                            "Cross-compaction replay failed — rewind aborted"
                        );
                        // Do NOT fall back to truncation: the post-compaction
                        // conversation has wrong user-message counts, and raw
                        // replay without a checkpoint produces an oversized
                        // conversation that will exceed the context window.
                        // Return a clear error so the user can rewind to a
                        // different (post-compaction) prompt instead.
                        return Ok(RewindResponse {
                            success: false,
                            target_prompt_index: target_index,
                            mode,
                            reverted_files: vec![],
                            clean_files: vec![],
                            conflicts: vec![],
                            prompt_text: None,
                            error: Some(format!(
                                "Cannot rewind to prompt #{} — compaction checkpoint data is \
                                 unavailable ({e}). Try rewinding to a prompt after the \
                                 compaction point instead.",
                                target_index,
                            )),
                        });
                    }
                }
            } else {
                // Standard rewind: truncate in-memory conversation. "Rewind
                // to N" = restore state before prompt N ran, keeping 0..N-1;
                // target 0 keeps only the session preamble.
                let keep_count = conversation_truncate_for_prompt(&conversation, target_index);
                conversation.truncate(keep_count);
            }

            // Write the truncated conversation back via the actor
            // (handles both state update + persistence).
            self.chat_state_handle.replace_conversation(conversation);
            // Use a snapshot to set the correct prompt_index and truncated prompt_texts.
            // The actor's TruncateToPromptIndex doesn't apply here because the
            // conversation was already truncated locally. Instead, snapshot + restore
            // with the corrected fields.
            if let Some(mut snap) = self.chat_state_handle.snapshot().await {
                snap.prompt_index = target_index;
                snap.prompt_texts.truncate(target_index);
                // Cross-compaction rewind recomputes the marker (the rebuilt
                // conversation may have dropped the summary); standard
                // truncation keeps the existing marker.
                let new_marker =
                    replay_compaction_marker.unwrap_or(snap.last_compaction_prompt_index);
                snap.last_compaction_prompt_index = new_marker;
                self.chat_state_handle.restore_snapshot(snap);
            }

            // Conversation shrank — clear budget-based (size/schema) and stale
            // per-turn suppression so compaction can run against the smaller context.
            // Account-state suppression (credit/auth → SUPPRESS_UNTIL_SUCCESS) isn't
            // budget-related, so it persists until a successful model call.
            if self
                .compaction
                .auto_compact_suppressed
                .load(std::sync::atomic::Ordering::Relaxed)
                != crate::session::compaction_config::SUPPRESS_UNTIL_SUCCESS
            {
                self.compaction.auto_compact_suppressed.store(
                    crate::session::compaction_config::SUPPRESS_NONE,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }

            // Append a RewindMarker to updates.jsonl so the replay pipeline can
            // handle timeline branching (updates.jsonl is append-only).
            self.persist_xai_update_only(XaiSessionUpdate::RewindMarker {
                target_prompt_index: target_index,
                created_at: chrono::Utc::now().to_rfc3339(),
            });
        }

        // Update the file state tracker to reflect the rewind.
        if wants_file_revert {
            // All/FilesOnly: files were reverted, snapshots are stale — truncate.
            self.file_state_tracker.truncate_from(target_index).await;
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::TruncateRewindPoints {
                    from_index: target_index,
                });
        } else if wants_conversation_rewind {
            // ConversationOnly: files are untouched but the conversation is rewound.
            self.merge_rewind_tracker_from(target_index).await;
        }

        Ok(RewindResponse {
            success: true,
            target_prompt_index: target_index,
            mode,
            reverted_files,
            clean_files: vec![],
            conflicts,
            prompt_text,
            error: None,
        })
    }

    /// `ConversationOnly` rewind-tracker bookkeeping: merge the discarded
    /// prompts' file effects (`>= target_index`) into the previous rewind point
    /// so that (a) `/rewind 0` can still undo all file changes, and (b) a new
    /// prompt at `target_index` gets a fresh rewind point whose before-snapshots
    /// reflect current disk state. Files and the conversation are left untouched.
    ///
    /// Updates the in-memory tracker, then persists via a disk-authoritative
    /// merge so a lazily-unloaded or partial tracker can't truncate history off
    /// disk. No normalize_to_relative needed: per-turn persistence already
    /// normalized the on-disk points (turn.rs, before PersistenceMsg::RewindPoint).
    ///
    /// Shared by local `handle_rewind` (ConversationOnly) and the bridge-mode
    /// ConversationOnly path, whose conversation rewind lands server-side
    /// (SessionCommand::ReconcileRewindTracker).
    pub(super) async fn merge_rewind_tracker_from(&self, target_index: usize) {
        self.file_state_tracker
            .merge_and_remove_from(target_index)
            .await;
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::MergeRewindPointsFrom { target_index });
    }

    /// Out-of-band history repair (`x.ai/session/repair`) for a resident
    /// session: run `xai_chat_state::compaction_utils::repair_history` inside
    /// the chat-state actor, then flush persistence so `chat_history.jsonl`
    /// is rewritten on disk before the caller sees success.
    ///
    /// Refused while a turn is in flight (in-flight tool calls legitimately
    /// await their results). The refusal is enforced inside the chat-state
    /// actor's command handler — see `ChatStateCommand::RepairHistory` for
    /// why a caller-side check alone would race turn start; the check below
    /// is just a fast path.
    pub(super) async fn handle_repair_history(
        &self,
        dry_run: bool,
    ) -> anyhow::Result<xai_chat_state::compaction_utils::HistoryRepairReport> {
        // Per-session flag — NOT `tool_context.is_turn_active`, which is the
        // agent-wide coordinator flag shared by all sessions (using it would
        // refuse repair of an idle session while any other session runs a
        // turn, and another session's turn end could clear it mid-turn).
        let turn_flag = self.session_turn_active.clone();
        if turn_flag.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!(xai_chat_state::commands::RepairHistoryBlocked);
        }

        let report = self
            .chat_state_handle
            .repair_history(dry_run, Some(turn_flag))
            .await
            .ok_or_else(|| anyhow::anyhow!("chat-state actor unavailable"))?
            .map_err(anyhow::Error::new)?;

        if report.changed() && !dry_run {
            // Flush barrier: success must mean the rewrite is on disk.
            let (flush_tx, flush_rx) = tokio::sync::oneshot::channel();
            if self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::FlushAndAck {
                    respond_to: flush_tx,
                })
                .is_err()
                || flush_rx.await.is_err()
            {
                anyhow::bail!("history repaired in memory but the persistence flush failed");
            }
            tracing::warn!(
                session_id = %self.session_info.id.0,
                duplicates_removed = report.duplicates_removed,
                stripped_tool_result_ids = ?report.stripped_tool_result_ids,
                synthetic_results_inserted = report.synthetic_results_inserted,
                "session history repaired"
            );
        }

        Ok(report)
    }
}
