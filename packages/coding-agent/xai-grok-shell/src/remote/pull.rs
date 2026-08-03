//! Pull-on-miss: fetch a session from the backend and hydrate local JSONL storage.

use crate::remote::client::{BackendClient, BackendError};

#[derive(Debug)]
pub enum PullResult {
    /// Written to local storage. The [`Info`] cwd comes from the backend (may differ from caller's).
    Hydrated(crate::session::info::Info),
    /// Not found on the backend.
    NotFound,
}

/// Fetch a session from the backend and hydrate local JSONL storage.
pub async fn pull_session_to_local(
    session_id: &str,
    client: &BackendClient,
) -> Result<PullResult, BackendError> {
    let loaded = match client.load_session_data(session_id).await {
        Ok(resp) => resp,
        Err(BackendError::SessionNotFound { .. }) => return Ok(PullResult::NotFound),
        Err(e) => return Err(e),
    };

    let remote = match loaded.session.as_ref() {
        Some(s) => s,
        None => return Ok(PullResult::NotFound),
    };

    // cwd required for local dir placement; null means pre-writeback session.
    let cwd = match remote.cwd.as_ref() {
        Some(cwd) => cwd,
        None => {
            tracing::warn!(session_id, "Cannot pull session: backend has cwd=null");
            return Ok(PullResult::NotFound);
        }
    };

    let info = crate::session::info::Info {
        id: agent_client_protocol::SessionId::new(std::sync::Arc::from(session_id)),
        cwd: cwd.clone(),
    };
    let dir = crate::session::persistence::session_dir(&info);

    let num_messages = hydrate::write_to_dir(&dir, &loaded)?;

    tracing::info!(session_id, %cwd, num_messages, "Pulled session from backend");

    Ok(PullResult::Hydrated(info))
}

pub(crate) mod hydrate {
    use std::path::Path;
    use std::sync::Arc;

    use crate::remote::client::{BackendError, LoadDataResponse, LoadedMessage, SessionInfo};
    use crate::session::info::Info;
    use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary, default_model_id};
    use crate::session::storage::{SUMMARY_FILE, UPDATES_FILE};

    fn io_err(path: &Path, source: std::io::Error) -> BackendError {
        BackendError::Hydration {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Write all session files to `dir`.
    pub(super) fn write_to_dir(
        dir: &Path,
        loaded: &LoadDataResponse,
    ) -> Result<usize, BackendError> {
        let remote = loaded
            .session
            .as_ref()
            .expect("caller checked session.is_some()");

        let info = Info {
            id: agent_client_protocol::SessionId::new(Arc::from(remote.session_id.as_str())),
            cwd: remote.cwd.clone().expect("caller verified cwd is Some"),
        };

        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;

        let num_messages = loaded.messages.as_ref().map_or(0, |m| m.len());
        let mut num_chat_messages = 0;

        if let Some(ref messages) = loaded.messages {
            write_updates(dir, messages)?;
            num_chat_messages = crate::session::storage::chat_rebuild::rebuild_chat_history(dir)
                .map_err(|e| io_err(dir, e))?;
        }

        write_summary(dir, &info, remote, num_messages, num_chat_messages)?;
        write_remote_origin_marker(dir);

        Ok(num_messages)
    }

    fn write_summary(
        dir: &Path,
        info: &Info,
        remote: &SessionInfo,
        num_messages: usize,
        num_chat_messages: usize,
    ) -> Result<(), BackendError> {
        let meta = remote.metadata.as_ref();

        let model_id = meta
            .and_then(|m| m.get("modelId"))
            .and_then(|v| v.as_str())
            .map(agent_client_protocol::ModelId::new)
            .unwrap_or_else(default_model_id);

        let parent_session_id = meta
            .and_then(|m| m.get("parentSessionId"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let summary = Summary {
            info: info.clone(),
            cwd_generation: 0,
            previous_cwd: None,
            pending_cwd_switch_reminder: None,
            cwd_switch_bookkeeping_generation: 0,
            session_summary: remote.title.clone().unwrap_or_default(),
            created_at: parse_rfc3339_or_now(remote.created_at.as_deref()),
            updated_at: parse_rfc3339_or_now(remote.updated_at.as_deref()),
            num_messages,
            num_chat_messages,
            current_model_id: model_id,
            parent_session_id,
            forked_at: None,
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: CHAT_FORMAT_VERSION,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            inherited_prefix_len: None,
            hidden: None,
            source_workspace_dir: None,
            git_root_dir: None,
            git_remotes: Vec::new(),
            head_commit: None,
            head_branch: None,
            request_id: None,
            // Record the *local* grok_home (where this hydrated copy lives),
            // not the original remote session's, since reconstruction runs locally.
            grok_home: crate::session::persistence::grok_home_string(),
            last_active_at: None,
            generated_title: None,
            title_is_manual: false,
            worktree_label: None,
            agent_name: None,
            // Hydrated locally — record the profile this process runs under.
            sandbox_profile: xai_grok_sandbox::configured_profile_name().map(String::from),
            reasoning_effort: None,
        };

        let json = serde_json::to_string_pretty(&summary)?;
        write_file(&dir.join(SUMMARY_FILE), json.as_bytes())
    }

    /// Convert backend JSON-RPC messages to local updates.jsonl (replayable methods only).
    pub(super) fn write_updates(
        dir: &Path,
        messages: &[LoadedMessage],
    ) -> Result<(), BackendError> {
        use std::io::Write;

        let path = dir.join(UPDATES_FILE);
        let file = std::fs::File::create(&path).map_err(|e| io_err(&path, e))?;
        let mut w = std::io::BufWriter::new(file);

        for msg in messages {
            let parsed = match serde_json::from_str::<serde_json::Value>(&msg.content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !is_session_update(&parsed) {
                continue;
            }
            if let Some(line) = to_envelope_line(&parsed) {
                let _ = w.write_all(line.as_bytes());
                let _ = w.write_all(b"\n");
            }
        }

        w.flush().map_err(|e| io_err(&path, e))
    }

    fn write_remote_origin_marker(dir: &Path) {
        let _ = std::fs::write(
            dir.join(".remote_origin"),
            format!("pulled_at={}\n", chrono::Utc::now().to_rfc3339()),
        );
    }

    /// Replayable JSON-RPC methods (excludes metadata like `prompt_complete`).
    const REPLAYABLE_METHODS: &[&str] = &["session/update", "_x.ai/session/update"];

    fn is_session_update(json_rpc: &serde_json::Value) -> bool {
        json_rpc
            .get("method")
            .and_then(|v| v.as_str())
            .is_some_and(|m| REPLAYABLE_METHODS.contains(&m))
    }

    fn to_envelope_line(json_rpc: &serde_json::Value) -> Option<String> {
        let method = json_rpc.get("method").and_then(|v| v.as_str())?;
        let params = json_rpc.get("params").cloned().unwrap_or_default();

        serde_json::to_string(&serde_json::json!({
            "timestamp": 0u64,
            "method": method,
            "params": params,
        }))
        .ok()
    }

    fn parse_rfc3339_or_now(s: Option<&str>) -> chrono::DateTime<chrono::Utc> {
        s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now)
    }

    fn write_file(path: &Path, data: &[u8]) -> Result<(), BackendError> {
        std::fs::write(path, data).map_err(|e| io_err(path, e))
    }
}

#[cfg(test)]
mod tests {
    use crate::remote::client::LoadedMessage;

    #[test]
    fn hydrate_writes_valid_updates_jsonl() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            LoadedMessage {
                id: "1".into(),
                content: r#"{"method":"session/update","params":{"update":"hello"}}"#.into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "2".into(),
                content: r#"{"method":"session/update","params":{"update":"world"}}"#.into(),
                timestamp: None,
            },
        ];

        super::hydrate::write_updates(tmp.path(), &messages).unwrap();

        let content = std::fs::read_to_string(tmp.path().join("updates.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["timestamp"], 0);
            assert_eq!(v["method"], "session/update");
            assert!(v["params"].is_object());
        }
    }

    #[test]
    fn rebuild_chat_history_merges_chunks() {
        use crate::session::export::ExportedMessage;
        use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
        use std::sync::Arc;

        // Build ACP notifications matching the RemoteSync path
        let sid = agent_client_protocol::SessionId::new(Arc::from("test"));
        let notifications = [
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("hello "),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("world"),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("hi back"),
                ))),
            ),
        ];

        // Serialize through ExportedMessage (writeback path)
        let messages: Vec<LoadedMessage> = notifications
            .iter()
            .map(|n| {
                let exported = ExportedMessage::from_notification(n);
                LoadedMessage {
                    id: "x".into(),
                    content: exported.content,
                    timestamp: None,
                }
            })
            .collect();

        let data = crate::remote::client::LoadDataResponse {
            messages: Some(messages),
            session: Some(crate::remote::client::SessionInfo {
                session_id: "test".into(),
                title: None,
                cwd: Some("/tmp".into()),
                status: None,
                created_at: None,
                updated_at: None,
                metadata: None,
            }),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        super::hydrate::write_to_dir(tmp.path(), &data).unwrap();

        let chat = std::fs::read_to_string(tmp.path().join("chat_history.jsonl")).unwrap();
        let items: Vec<crate::sampling::ConversationItem> = chat
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        assert_eq!(items.len(), 2, "should have 1 user + 1 agent item");
        assert!(matches!(
            &items[0],
            crate::sampling::ConversationItem::User(_)
        ));
        assert!(matches!(
            &items[1],
            crate::sampling::ConversationItem::Assistant(_)
        ));
        if let crate::sampling::ConversationItem::User(u) = &items[0] {
            let text: String = u
                .content
                .iter()
                .filter_map(|p| match p {
                    crate::sampling::ContentPart::Text { text } => Some(text.as_ref()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "hello world");
        }
    }

    #[test]
    fn rebuild_chat_history_preserves_user_images() {
        use crate::session::export::ExportedMessage;
        use agent_client_protocol::{
            ContentBlock, ContentChunk, ImageContent, SessionUpdate, TextContent,
        };
        use std::sync::Arc;

        let sid = agent_client_protocol::SessionId::new(Arc::from("test"));
        let notifications = [
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("look at this"),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Image(
                    ImageContent::new(String::new(), String::new())
                        .uri(Some("data:image/png;base64,abc".into())),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("I see an image"),
                ))),
            ),
        ];

        let messages: Vec<LoadedMessage> = notifications
            .iter()
            .map(|n| LoadedMessage {
                id: "x".into(),
                content: ExportedMessage::from_notification(n).content,
                timestamp: None,
            })
            .collect();

        let data = crate::remote::client::LoadDataResponse {
            messages: Some(messages),
            session: Some(crate::remote::client::SessionInfo {
                session_id: "test".into(),
                title: None,
                cwd: Some("/tmp".into()),
                status: None,
                created_at: None,
                updated_at: None,
                metadata: None,
            }),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        super::hydrate::write_to_dir(tmp.path(), &data).unwrap();

        let chat = std::fs::read_to_string(tmp.path().join("chat_history.jsonl")).unwrap();
        let items: Vec<crate::sampling::ConversationItem> = chat
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        assert_eq!(items.len(), 2);
        if let crate::sampling::ConversationItem::User(u) = &items[0] {
            assert_eq!(u.content.len(), 2, "should have text + image parts");
            assert!(matches!(
                &u.content[0],
                crate::sampling::ContentPart::Text { .. }
            ));
            assert!(matches!(
                &u.content[1],
                crate::sampling::ContentPart::Image { .. }
            ));
        } else {
            panic!("expected User item");
        }
    }

    #[test]
    fn hydrate_skips_invalid_messages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            LoadedMessage {
                id: "1".into(),
                content: r#"{"method":"session/update","params":{}}"#.into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "bad".into(),
                content: "not valid json".into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "3".into(),
                content: r#"{"method":"session/update","params":{"x":1}}"#.into(),
                timestamp: None,
            },
        ];

        super::hydrate::write_updates(tmp.path(), &messages).unwrap();

        let content = std::fs::read_to_string(tmp.path().join("updates.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "invalid message should be skipped");
    }
}
