//! Session export for sharing via the remote session-sharing backend.
//!
//! Uses `updates.jsonl` (ACP SessionNotifications) as the source of truth,
//! not `chat_history.jsonl` which is only for LLM API calls.

use crate::session::info::Info;
use crate::session::persistence::Summary;
use crate::session::storage::{JsonlStorageAdapter, PersistedData, SessionUpdate, StorageAdapter};
use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

/// JSON-RPC wrapper for ACP notifications.
#[derive(Debug, Serialize)]
struct AcpJsonRpcNotification<'a> {
    method: &'static str,
    params: &'a acp::SessionNotification,
}

/// JSON-RPC wrapper for xAI extension notifications.
#[derive(Debug, Serialize)]
struct XaiJsonRpcNotification<'a> {
    method: &'static str,
    params: &'a crate::extensions::notification::SessionNotification,
}

const ACP_SESSION_UPDATE_METHOD: &str = "session/update";
const XAI_SESSION_UPDATE_METHOD: &str = "_x.ai/session/update";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedMessage {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

impl ExportedMessage {
    pub fn from_notification(notification: &acp::SessionNotification) -> Self {
        let wrapper = AcpJsonRpcNotification {
            method: ACP_SESSION_UPDATE_METHOD,
            params: notification,
        };
        let content = serde_json::to_string(&wrapper).unwrap_or_else(|_| "{}".to_string());

        let timestamp = notification
            .meta
            .as_ref()
            .and_then(|m| m.get("timestamp"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Self { content, timestamp }
    }

    pub(crate) fn from_xai_notification(
        notification: &crate::extensions::notification::SessionNotification,
    ) -> Self {
        let wrapper = XaiJsonRpcNotification {
            method: XAI_SESSION_UPDATE_METHOD,
            params: notification,
        };
        let content = serde_json::to_string(&wrapper).unwrap_or_else(|_| "{}".to_string());

        let timestamp = notification
            .meta
            .as_ref()
            .and_then(|m| m.get("timestamp"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Self { content, timestamp }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_messages: Option<usize>,
    /// Parent session ID if this session was forked from another session
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,

    // --- Subagent-specific fields (all optional for backward compatibility) ---
    /// Session kind: "parent", "subagent", or "subagent_fork".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// Subagent type (e.g., "general-purpose", "explore", "plan").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    /// Named persona applied to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_persona: Option<String>,
    /// Named role applied to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_role: Option<String>,
    /// Effective context source ("new" or "resumed").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_context_source: Option<String>,
    /// Subagent nesting depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth: Option<u32>,
}

impl ExportedMetadata {
    /// Build metadata from a [`Summary`].
    pub(crate) fn from_summary(summary: &Summary) -> Self {
        Self {
            title: Some(summary.session_summary.clone()).filter(|s| !s.is_empty()),
            cwd: summary.info.cwd.clone(),
            model_id: Some(summary.current_model_id.0.to_string()),
            created_at: Some(summary.created_at.to_rfc3339()),
            updated_at: Some(summary.updated_at.to_rfc3339()),
            total_messages: Some(summary.num_messages),
            parent_session_id: summary.parent_session_id.clone(),
            session_kind: None,
            subagent_type: None,
            subagent_persona: None,
            subagent_role: None,
            fork_context_source: None,
            subagent_depth: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedSession {
    pub session_id: String,
    pub messages: Vec<ExportedMessage>,
    pub metadata: ExportedMetadata,
}

impl ExportedSession {
    pub(crate) fn from_persisted_data(info: &Info, data: &PersistedData) -> Self {
        let messages = Self::convert_updates(&data.updates);
        let metadata = ExportedMetadata::from_summary(&data.summary);

        Self {
            session_id: info.id.to_string(),
            messages,
            metadata,
        }
    }

    pub(crate) async fn from_local_session(info: &Info) -> std::io::Result<Self> {
        let storage = JsonlStorageAdapter::new();
        let data = storage.load_session(info).await?;
        Ok(Self::from_persisted_data(info, &data))
    }

    fn convert_updates(updates: &[SessionUpdate]) -> Vec<ExportedMessage> {
        updates
            .iter()
            .map(|update| match update {
                SessionUpdate::Acp(notification) => {
                    ExportedMessage::from_notification(notification)
                }
                SessionUpdate::Xai(notification) => {
                    ExportedMessage::from_xai_notification(notification)
                }
            })
            .collect()
    }
}
