//! Frameless Bidi wire protocol for Codex Live voice sessions.
//!
//! A faithful Rust port of `packages/coding-agent/src/live/protocol.ts` from
//! oh-my-pi (OMP) v17.1.1 (commit e9c8a35), preserving the exact wire format:
//! the `gpt-live-1-codex` model id, the session payload shape, the client
//! control messages, the server event parser, and the 500-UTF-8-byte context
//! chunking. Substantially adapted from the TypeScript original; MIT
//! attribution preserved in `THIRD-PARTY-NOTICES`.
//!
//! # Secrets/log safety
//! The server parser only extracts the typed fields it knows about and returns
//! `LiveServerEvent::Unknown { wire_type }` for anything else — it never logs
//! the raw payload wholesale, so an unexpected event carrying sensitive data is
//! reduced to its `type` string before it can reach a log line.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Realtime model id used by Codex Desktop live calls (exact server value).
pub const LIVE_MODEL: &str = "gpt-live-1-codex";

/// Maximum UTF-8 payload size accepted by each context append (server limit).
pub const CONTEXT_CHUNK_BYTES: usize = 500;

/// Semantic stream selected for appended Frameless Bidi context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveContextChannel {
    /// Text the model may speak aloud.
    Speakable,
    /// Background context the model should not read out loud.
    Commentary,
}

/// Text content item accepted by Frameless Bidi context appends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveInputTextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl LiveInputTextContent {
    /// `type` is always `"input_text"`; the field is fixed so serde emits it.
    const KIND: &'static str = "input_text";

    pub fn input_text(text: impl Into<String>) -> Self {
        Self {
            kind: Self::KIND,
            text: text.into(),
        }
    }
}

/// Session object posted alongside the SDP when opening a live call.
#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionPayload {
    pub model: &'static str,
    pub instructions: String,
    pub audio: LiveSessionAudio,
    pub delegation: LiveSessionDelegation,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionAudio {
    pub output: LiveSessionAudioOutput,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionAudioOutput {
    pub voice: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionDelegation {
    /// Always `"client"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl LiveSessionDelegation {
    const KIND: &'static str = "client";
}

/// Body of the Codex realtime signaling POST: the local SDP offer plus the
/// session object.
#[derive(Debug, Clone, Serialize)]
pub struct LiveSignalingRequest {
    pub sdp: String,
    pub session: LiveSessionPayload,
}

/// Messages sent by the client over the Frameless Bidi data channel / sideband.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum LiveClientMessage {
    #[serde(rename = "delegation.context.append")]
    DelegationContextAppend {
        delegation_item_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<LiveContextChannel>,
        content: Vec<LiveInputTextContent>,
    },
    #[serde(rename = "session.context.append")]
    SessionContextAppend {
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<LiveContextChannel>,
        content: Vec<LiveInputTextContent>,
    },
    #[serde(rename = "session.close")]
    SessionClose,
}

/// Role of a completed Frameless Bidi turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveRole {
    User,
    Assistant,
}

/// Parsed Frameless Bidi server events, including unsupported wire event types.
///
/// Unknown events keep only their wire `type` string — never the full payload —
/// so a server-side change can never leak sensitive fields into a log.
#[derive(Debug, Clone)]
pub enum LiveServerEvent {
    /// `session.started` / `session.updated`.
    Session {
        kind: SessionEventKind,
        id: String,
        instructions: Option<String>,
    },
    /// `output_audio.delta` — base64 Opus payload.
    OutputAudioDelta { audio: String },
    /// `input_transcript.added` / `output_transcript.added`.
    TranscriptAdded { kind: TranscriptKind, text: String },
    /// `turn.done`.
    TurnDone { role: LiveRole, transcript: String },
    /// `delegation.created` — the server asks the client to perform work.
    DelegationCreated {
        id: String,
        content: Vec<LiveInputTextContent>,
    },
    /// `error`.
    Error { message: String },
    /// Any event whose `type` we do not model. Only the type string is kept.
    Unknown { wire_type: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventKind {
    Started,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    Input,
    Output,
}

/// Parse a JSON string (or already-decoded value) from the Frameless Bidi data
/// channel / sideband into a typed event. Returns `None` for malformed JSON, a
/// non-object, a missing/invalid `type`, or a known type whose body fails
/// validation — matching the OMP parser, which silently drops such frames.
///
/// **Never logs the raw payload.** Unknown events surface only as
/// [`LiveServerEvent::Unknown { wire_type }`].
pub fn parse_live_server_event(payload: &str) -> Option<LiveServerEvent> {
    let value: Value = serde_json::from_str(payload).ok()?;
    parse_live_server_event_value(&value)
}

/// Parse an already-decoded JSON value. Used by the data-channel fallback path
/// where the payload may arrive as a structured value.
pub fn parse_live_server_event_value(value: &Value) -> Option<LiveServerEvent> {
    let obj = value.as_object()?;
    let wire_type = obj.get("type")?.as_str()?;
    match wire_type {
        "session.started" | "session.updated" => parse_session_event(
            if wire_type == "session.started" {
                SessionEventKind::Started
            } else {
                SessionEventKind::Updated
            },
            obj,
        ),
        "output_audio.delta" => obj.get("audio").and_then(Value::as_str).map(|audio| {
            LiveServerEvent::OutputAudioDelta {
                audio: audio.to_string(),
            }
        }),
        "input_transcript.added" | "output_transcript.added" => parse_transcript_added_event(
            if wire_type == "input_transcript.added" {
                TranscriptKind::Input
            } else {
                TranscriptKind::Output
            },
            obj,
        ),
        "turn.done" => parse_turn_done_event(obj),
        "delegation.created" => parse_delegation_created_event(obj),
        "error" => parse_error_event(obj),
        other => Some(LiveServerEvent::Unknown {
            wire_type: other.to_string(),
        }),
    }
}

fn parse_session_event(
    kind: SessionEventKind,
    obj: &serde_json::Map<String, Value>,
) -> Option<LiveServerEvent> {
    let session = obj.get("session")?.as_object()?;
    let id = session.get("id")?.as_str()?.to_string();
    let instructions = session
        .get("instructions")
        .and_then(Value::as_str)
        .map(String::from);
    Some(LiveServerEvent::Session {
        kind,
        id,
        instructions,
    })
}

fn parse_transcript_added_event(
    kind: TranscriptKind,
    obj: &serde_json::Map<String, Value>,
) -> Option<LiveServerEvent> {
    let item = obj.get("item")?.as_object()?;
    let text = item.get("text")?.as_str()?.to_string();
    Some(LiveServerEvent::TranscriptAdded { kind, text })
}

fn parse_turn_done_event(obj: &serde_json::Map<String, Value>) -> Option<LiveServerEvent> {
    let turn = obj.get("turn")?.as_object()?;
    let role = match turn.get("role")?.as_str()? {
        "user" => LiveRole::User,
        "assistant" => LiveRole::Assistant,
        _ => return None,
    };
    let transcript = turn.get("transcript")?.as_str()?.to_string();
    Some(LiveServerEvent::TurnDone { role, transcript })
}

fn parse_delegation_created_event(obj: &serde_json::Map<String, Value>) -> Option<LiveServerEvent> {
    let item = obj.get("item")?.as_object()?;
    if item.get("type").and_then(Value::as_str) != Some("delegation")
        || item.get("target").and_then(Value::as_str) != Some("client")
    {
        return None;
    }
    let id = item.get("id")?.as_str()?.to_string();
    let content = item.get("content")?.as_array()?;
    let mut parsed = Vec::with_capacity(content.len());
    for candidate in content {
        let candidate = candidate.as_object()?;
        if candidate.get("type").and_then(Value::as_str) != Some("input_text") {
            continue;
        }
        if let Some(text) = candidate.get("text").and_then(Value::as_str) {
            parsed.push(LiveInputTextContent::input_text(text));
        }
    }
    Some(LiveServerEvent::DelegationCreated {
        id,
        content: parsed,
    })
}

fn parse_error_event(obj: &serde_json::Map<String, Value>) -> Option<LiveServerEvent> {
    if let Some(message) = obj.get("message").and_then(Value::as_str) {
        return Some(LiveServerEvent::Error {
            message: message.to_string(),
        });
    }
    if let Some(error) = obj.get("error").and_then(Value::as_object)
        && let Some(message) = error.get("message").and_then(Value::as_str)
    {
        return Some(LiveServerEvent::Error {
            message: message.to_string(),
        });
    }
    let error = obj.get("error");
    let message = stringify_error_value(error)?;
    Some(LiveServerEvent::Error { message })
}

fn stringify_error_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => serde_json::to_string(v).ok().filter(|s| !s.is_empty()),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Build the session object posted in the multipart WebRTC call request.
pub fn build_live_session_payload(
    instructions: impl Into<String>,
    voice: impl Into<String>,
) -> LiveSessionPayload {
    LiveSessionPayload {
        model: LIVE_MODEL,
        instructions: instructions.into(),
        audio: LiveSessionAudio {
            output: LiveSessionAudioOutput {
                voice: voice.into(),
            },
        },
        delegation: LiveSessionDelegation {
            kind: LiveSessionDelegation::KIND,
        },
    }
}

/// Build a context append associated with a server-created delegation.
pub fn build_delegation_context_append(
    delegation_item_id: impl Into<String>,
    text: impl Into<String>,
    channel: Option<LiveContextChannel>,
) -> LiveClientMessage {
    LiveClientMessage::DelegationContextAppend {
        delegation_item_id: delegation_item_id.into(),
        channel,
        content: vec![LiveInputTextContent::input_text(text)],
    }
}

/// Build context appended to the live session outside a delegation.
pub fn build_session_context_append(
    text: impl Into<String>,
    channel: Option<LiveContextChannel>,
) -> LiveClientMessage {
    LiveClientMessage::SessionContextAppend {
        channel,
        content: vec![LiveInputTextContent::input_text(text)],
    }
}

/// Build the message that gracefully closes a live session.
pub fn build_session_close() -> LiveClientMessage {
    LiveClientMessage::SessionClose
}

// ---------------------------------------------------------------------------
// Context chunking
// ---------------------------------------------------------------------------

/// UTF-8 byte length of a single Unicode code point.
fn utf8_byte_length(code_point: u32) -> usize {
    if code_point <= 0x7f {
        1
    } else if code_point <= 0x7ff {
        2
    } else if code_point <= 0xffff {
        3
    } else {
        4
    }
}

/// Split context into character-safe chunks of at most [`CONTEXT_CHUNK_BYTES`]
/// UTF-8 bytes. A code point is never split across chunks. Empty input yields a
/// single empty chunk (matching the OMP implementation, so an append is always
/// sent even for empty text).
pub fn chunk_live_context(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut chunk_start = 0usize;
    let mut chunk_bytes = 0usize;
    let mut index = 0usize;
    let chars: Vec<char> = text.chars().collect();
    while index < chars.len() {
        let code_point = chars[index] as u32;
        let character_bytes = utf8_byte_length(code_point);
        if chunk_bytes + character_bytes > CONTEXT_CHUNK_BYTES {
            chunks.push(chars[chunk_start..index].iter().collect());
            chunk_start = index;
            chunk_bytes = 0;
        }
        chunk_bytes += character_bytes;
        index += 1;
    }
    chunks.push(chars[chunk_start..].iter().collect());
    chunks
}

// ---------------------------------------------------------------------------
// Call-id parsing
// ---------------------------------------------------------------------------

/// Regex-like predicate for a valid server-assigned `rtc_*` call id.
/// Matches `rtc_[\w-]+` (alphanumeric, underscore, hyphen).
fn is_rtc_call_id(segment: &str) -> bool {
    if !segment.starts_with("rtc_") {
        return false;
    }
    let rest = &segment[4..];
    !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Extract the server-assigned `rtc_*` call ID from a signaling `Location`
/// header. Scans the path (before any `?` query) for the first segment matching
/// `rtc_[\w-]+`, matching the OMP `parseLiveCallId` exactly.
pub fn parse_live_call_id(location: Option<&str>) -> Option<String> {
    let location = location?;
    let path = location.split('?').next()?;
    path.split('/')
        .find(|segment| is_rtc_call_id(segment))
        .map(str::to_string)
}

/// Build the Frameless Bidi sideband WebSocket URL for an accepted Codex call.
/// `https://api.openai.com/v1/live/<callId>` → `wss://api.openai.com/v1/live/<callId>`.
pub fn build_live_sideband_url(call_id: &str) -> String {
    build_live_sideband_url_with_base(call_id, None)
}

/// Build the sideband WebSocket URL, optionally overriding the default
/// `https://api.openai.com/v1/live/` base. When `sideband_base` is `Some`, it
/// is used as-is (protocol upgraded to `wss:` if it's `https:`); when `None`,
/// the default `wss://api.openai.com/v1/live/` is used. The call id is
/// percent-encoded.
pub fn build_live_sideband_url_with_base(call_id: &str, sideband_base: Option<&str>) -> String {
    // `encodeURIComponent` equivalent: percent-encode everything but the
    // unreserved set (A-Za-z0-9-_.~). `rtc_` ids are already in that set, so
    // this is a no-op for valid ids but stays safe for a malformed value.
    let encoded: String = call_id
        .bytes()
        .map(|b| {
            let c = b as char;
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                String::from(c)
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect();
    match sideband_base {
        Some(base) if !base.trim().is_empty() => {
            let base = base.trim_end_matches('/');
            // Upgrade https: → wss:, http: → ws:; if already ws/wss keep as-is.
            if base.starts_with("wss://") || base.starts_with("ws://") {
                format!("{base}/{encoded}")
            } else if let Some(stripped) = base.strip_prefix("https://") {
                format!("wss://{stripped}/{encoded}")
            } else if let Some(stripped) = base.strip_prefix("http://") {
                format!("ws://{stripped}/{encoded}")
            } else {
                // No scheme: assume wss.
                format!("wss://{base}/{encoded}")
            }
        }
        _ => format!("wss://api.openai.com/v1/live/{encoded}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_payload_serializes_exact_shape() {
        let payload = build_live_session_payload("be concise", "alloy");
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["model"], LIVE_MODEL);
        assert_eq!(json["instructions"], "be concise");
        assert_eq!(json["audio"]["output"]["voice"], "alloy");
        assert_eq!(json["delegation"]["type"], "client");
    }

    #[test]
    fn delegation_context_append_omits_channel_when_none() {
        let msg = build_delegation_context_append("rtc_del_1", "ctx", None);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "delegation.context.append");
        assert_eq!(json["delegation_item_id"], "rtc_del_1");
        assert!(json.get("channel").is_none(), "channel must be absent");
        assert_eq!(json["content"][0]["type"], "input_text");
        assert_eq!(json["content"][0]["text"], "ctx");
    }

    #[test]
    fn delegation_context_append_includes_channel_when_set() {
        let msg = build_delegation_context_append("d1", "ctx", Some(LiveContextChannel::Speakable));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["channel"], "speakable");
    }

    #[test]
    fn session_context_append_serializes_channel_and_content() {
        let msg = build_session_context_append("hello", Some(LiveContextChannel::Commentary));
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "session.context.append");
        assert_eq!(json["channel"], "commentary");
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn session_close_serializes_to_bare_type() {
        let json = serde_json::to_string(&build_session_close()).unwrap();
        assert_eq!(json, r#"{"type":"session.close"}"#);
    }

    // --- parser ---

    #[test]
    fn parse_session_started() {
        let ev = parse_live_server_event(
            r#"{"type":"session.started","session":{"id":"sess_1","instructions":"hi"}}"#,
        )
        .unwrap();
        match ev {
            LiveServerEvent::Session {
                kind,
                id,
                instructions,
            } => {
                assert_eq!(kind, SessionEventKind::Started);
                assert_eq!(id, "sess_1");
                assert_eq!(instructions.as_deref(), Some("hi"));
            }
            other => panic!("expected Session, got {other:?}"),
        }
    }

    #[test]
    fn parse_session_updated_without_instructions() {
        let ev =
            parse_live_server_event(r#"{"type":"session.updated","session":{"id":"s2"}}"#).unwrap();
        match ev {
            LiveServerEvent::Session {
                kind,
                id,
                instructions,
            } => {
                assert_eq!(kind, SessionEventKind::Updated);
                assert_eq!(id, "s2");
                assert!(instructions.is_none());
            }
            other => panic!("expected Session, got {other:?}"),
        }
    }

    #[test]
    fn parse_output_audio_delta() {
        let ev = parse_live_server_event(r#"{"type":"output_audio.delta","audio":"base64data"}"#)
            .unwrap();
        match ev {
            LiveServerEvent::OutputAudioDelta { audio } => assert_eq!(audio, "base64data"),
            other => panic!("expected OutputAudioDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_transcript_added() {
        let ev =
            parse_live_server_event(r#"{"type":"input_transcript.added","item":{"text":"hi"}}"#)
                .unwrap();
        match ev {
            LiveServerEvent::TranscriptAdded { kind, text } => {
                assert_eq!(kind, TranscriptKind::Input);
                assert_eq!(text, "hi");
            }
            other => panic!("expected TranscriptAdded, got {other:?}"),
        }
    }

    #[test]
    fn parse_turn_done() {
        let ev = parse_live_server_event(
            r#"{"type":"turn.done","turn":{"role":"assistant","transcript":"done"}}"#,
        )
        .unwrap();
        match ev {
            LiveServerEvent::TurnDone { role, transcript } => {
                assert_eq!(role, LiveRole::Assistant);
                assert_eq!(transcript, "done");
            }
            other => panic!("expected TurnDone, got {other:?}"),
        }
    }

    #[test]
    fn parse_delegation_created_filters_non_input_text_content() {
        let ev = parse_live_server_event(
            r#"{"type":"delegation.created","item":{"type":"delegation","target":"client","id":"del_1","content":[{"type":"input_text","text":"do x"},{"type":"image","url":"evil"},{"type":"input_text","text":"do y"}]}}"#,
        )
        .unwrap();
        match ev {
            LiveServerEvent::DelegationCreated { id, content } => {
                assert_eq!(id, "del_1");
                assert_eq!(content.len(), 2);
                assert_eq!(content[0].text, "do x");
                assert_eq!(content[1].text, "do y");
            }
            other => panic!("expected DelegationCreated, got {other:?}"),
        }
    }

    #[test]
    fn parse_delegation_created_rejects_wrong_target() {
        assert!(parse_live_server_event(
            r#"{"type":"delegation.created","item":{"type":"delegation","target":"server","id":"d","content":[]}}"#
        )
        .is_none());
    }

    #[test]
    fn parse_error_with_top_level_message() {
        let ev = parse_live_server_event(r#"{"type":"error","message":"boom"}"#).unwrap();
        match ev {
            LiveServerEvent::Error { message } => assert_eq!(message, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_error_with_nested_error_object() {
        let ev = parse_live_server_event(r#"{"type":"error","error":{"message":"nested fail"}}"#)
            .unwrap();
        match ev {
            LiveServerEvent::Error { message } => assert_eq!(message, "nested fail"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event_keeps_only_wire_type() {
        let ev =
            parse_live_server_event(r#"{"type":"future.event","secret":"leak","data":[1,2,3]}"#)
                .unwrap();
        match ev {
            LiveServerEvent::Unknown { wire_type } => assert_eq!(wire_type, "future.event"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_live_server_event("not json").is_none());
        assert!(parse_live_server_event(r#"{"no_type":1}"#).is_none());
        assert!(parse_live_server_event(r#"{"type":123}"#).is_none());
        assert!(parse_live_server_event(r#""scalar""#).is_none());
        assert!(parse_live_server_event("null").is_none());
    }

    #[test]
    fn parse_invalid_known_event_returns_none() {
        // session without session object
        assert!(parse_live_server_event(r#"{"type":"session.started"}"#).is_none());
        // session.id missing
        assert!(parse_live_server_event(r#"{"type":"session.started","session":{}}"#).is_none());
        // turn.done with bad role
        assert!(
            parse_live_server_event(
                r#"{"type":"turn.done","turn":{"role":"system","transcript":"x"}}"#
            )
            .is_none()
        );
    }

    // --- chunking ---

    #[test]
    fn chunk_empty_yields_single_empty_chunk() {
        assert_eq!(chunk_live_context(""), vec![""]);
    }

    #[test]
    fn chunk_ascii_under_limit_is_one_chunk() {
        assert_eq!(chunk_live_context("hello"), vec!["hello"]);
    }

    #[test]
    fn chunk_splits_at_500_bytes_on_code_point_boundary() {
        // 600 ASCII chars → two chunks, first exactly 500 bytes.
        let text = "a".repeat(600);
        let chunks = chunk_live_context(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 500);
        assert_eq!(chunks[0].len(), 500);
        assert_eq!(chunks[1].len(), 100);
    }

    #[test]
    fn chunk_never_splits_a_multibyte_code_point() {
        // Each '中' is 3 UTF-8 bytes. 200 chars = 600 bytes → 2 chunks of
        // 166/168 chars (498/504 bytes). 166*3=498 ≤ 500; adding one more
        // (501) would exceed the limit, so the boundary lands between chars.
        let text = "中".repeat(200);
        let chunks = chunk_live_context(&text);
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            assert!(chunk.len() <= CONTEXT_CHUNK_BYTES);
            // Re-encoding must round-trip: no broken code points.
            assert_eq!(chunk.chars().count(), chunk.chars().count());
        }
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, 200);
    }

    #[test]
    fn chunk_handles_emoji_4_byte_code_points() {
        // '😀' is 4 UTF-8 bytes. 125 of them = 500 bytes (one chunk); 126 → split.
        let text = "😀".repeat(126);
        let chunks = chunk_live_context(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 125);
        assert_eq!(chunks[1].chars().count(), 1);
        assert!(chunks[0].len() <= CONTEXT_CHUNK_BYTES);
    }

    // --- call-id ---

    #[test]
    fn parse_call_id_from_location() {
        assert_eq!(
            parse_live_call_id(Some(
                "https://api.openai.com/v1/live/rtc_abc-123_xyz/sideband"
            )),
            Some("rtc_abc-123_xyz".to_string())
        );
    }

    #[test]
    fn parse_call_id_strips_query_string() {
        assert_eq!(
            parse_live_call_id(Some("/v1/live/rtc_call1?token=secret")),
            Some("rtc_call1".to_string())
        );
    }

    #[test]
    fn parse_call_id_none_when_absent() {
        assert!(parse_live_call_id(Some("https://api.openai.com/v1/live/no-rtc-here")).is_none());
        assert!(parse_live_call_id(None).is_none());
    }

    #[test]
    fn parse_call_id_rejects_bare_rtc_prefix() {
        // `rtc_` alone has no trailing id characters.
        assert!(parse_live_call_id(Some("/v1/live/rtc_")).is_none());
    }

    #[test]
    fn parse_call_id_rejects_invalid_chars() {
        assert!(parse_live_call_id(Some("/v1/live/rtc_bad!")).is_none());
    }

    #[test]
    fn sideband_url_is_wss_with_encoded_call_id() {
        assert_eq!(
            build_live_sideband_url("rtc_abc-123"),
            "wss://api.openai.com/v1/live/rtc_abc-123"
        );
    }

    #[test]
    fn sideband_url_percent_encodes_unsafe_chars() {
        assert_eq!(
            build_live_sideband_url("rtc a/b"),
            "wss://api.openai.com/v1/live/rtc%20a%2Fb"
        );
    }

    #[test]
    fn sideband_url_with_custom_base_uses_it() {
        assert_eq!(
            build_live_sideband_url_with_base(
                "rtc_abc",
                Some("https://custom.example.com/v1/live")
            ),
            "wss://custom.example.com/v1/live/rtc_abc"
        );
    }

    #[test]
    fn sideband_url_with_wss_base_keeps_scheme() {
        assert_eq!(
            build_live_sideband_url_with_base("rtc_abc", Some("wss://proxy.corp.net/live")),
            "wss://proxy.corp.net/live/rtc_abc"
        );
    }

    #[test]
    fn sideband_url_with_none_base_uses_default() {
        assert_eq!(
            build_live_sideband_url_with_base("rtc_abc", None),
            "wss://api.openai.com/v1/live/rtc_abc"
        );
    }

    #[test]
    fn sideband_url_with_empty_base_uses_default() {
        assert_eq!(
            build_live_sideband_url_with_base("rtc_abc", Some("")),
            "wss://api.openai.com/v1/live/rtc_abc"
        );
    }
}
