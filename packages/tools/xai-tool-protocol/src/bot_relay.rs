//! Bot-relay wire types: JSON-RPC method payloads, the closed hub error
//! enum, and the `bot.event` envelope.
//!
//! These types are the client-facing stability boundary. Gateway command
//! names and payloads pass through verbatim; this module does not type them.

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use typeshare::typeshare;

use crate::{JsonRpcError, Method};

/// Wire method strings a `hello_ack` `capabilities` list advertises for
/// the bot-relay surface.
pub const BOT_RELAY_CAPABILITIES: &[&str] = &[
    Method::BotCommand.as_wire_str(),
    Method::BotVncDescriptor.as_wire_str(),
    Method::BotRoster.as_wire_str(),
    Method::BotStatus.as_wire_str(),
    Method::BotTranscriptOffbox.as_wire_str(),
    Method::BotSubscribe.as_wire_str(),
    Method::BotUnsubscribe.as_wire_str(),
    Method::BotBindConversation.as_wire_str(),
    Method::BotEvent.as_wire_str(),
];

/// `bot.event` envelope version carried in [`BotEventEnvelope::v`].
pub const BOT_EVENT_ENVELOPE_V: u32 = 1;

/// `reason` on `command_rejected` when the command is compiled in but not allowlisted.
pub const COMMAND_REJECTED_NOT_YET_ENABLED: &str = "not_yet_enabled";

/// `reason` on `command_rejected` when envelope `agentId` and `args.agentId` disagree.
pub const COMMAND_REJECTED_AGENT_ID_MISMATCH: &str = "agent_id_mismatch";

// ── Shared empty payloads ────────────────────────────────────────────────

/// Empty JSON object (`{}`). Used by verbs whose design params/result are `{}`.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotEmptyParams {}

/// Empty JSON object (`{}`). Alias of [`BotEmptyParams`].
#[typeshare]
pub type BotEmptyResult = BotEmptyParams;

// ── bot.command ──────────────────────────────────────────────────────────

/// `bot.command` params. `name` and `args` are upstream-verbatim.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotCommandParams {
    pub agent_id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Upstream-verbatim command result. Not interpreted by this crate.
#[typeshare]
pub type BotCommandResult = serde_json::Value;

// ── bot.vncDescriptor ────────────────────────────────────────────────────

/// `bot.vncDescriptor` params.
#[typeshare]
pub type BotVncDescriptorParams = BotEmptyParams;

/// `bot.vncDescriptor` result.
///
/// `expires_hint` is unix milliseconds. `null` means a legacy network-token
/// URL (valid until pod migration); a concrete value is the port-token
/// expiry the client should refresh before.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotVncDescriptorResult {
    pub vnc_url: String,
    /// Unix time in milliseconds, or `null` when the URL has no expiry.
    /// Present as `null` on the wire when unset; omitted keys also read as `None`.
    #[serde(default)]
    #[typeshare(serialized_as = "Option<NullableMillis>")]
    pub expires_hint: Option<i64>,
}

// ── bot.roster ───────────────────────────────────────────────────────────

/// `bot.roster` params. Cold — never wakes the box.
#[typeshare]
pub type BotRosterParams = BotEmptyParams;

/// One cached roster row.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRosterEntry {
    pub agent_id: String,
    pub name: String,
    pub status: String,
    /// Unix time in milliseconds of the agent's last turn, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<I54>")]
    pub last_turn_at: Option<i64>,
}

/// `bot.roster` result.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRosterResult {
    pub agents: Vec<BotRosterEntry>,
}

// ── bot.status ───────────────────────────────────────────────────────────

/// `bot.status` params. Cold — never wakes the box.
#[typeshare]
pub type BotStatusParams = BotEmptyParams;

/// Off-box run state. Senders emit only the named variants. Receivers
/// treat any unknown wire string as [`Self::Unknown`].
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRunState {
    Absent,
    Hibernated,
    Running,
    Unknown,
}

impl BotRunState {
    pub const ALL: &'static [Self] =
        &[Self::Absent, Self::Hibernated, Self::Running, Self::Unknown];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Hibernated => "hibernated",
            Self::Running => "running",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "absent" => Self::Absent,
            "hibernated" => Self::Hibernated,
            "running" => Self::Running,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for BotRunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotRunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// `bot.status` result: off-box run state.
///
/// `runState` is a string on the wire. Generated clients see `string` and
/// compare against [`BotRunState`]. Unknown values degrade to `unknown`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotStatusResult {
    #[typeshare(serialized_as = "String")]
    pub run_state: BotRunState,
}

// ── bot.transcript.offbox ────────────────────────────────────────────────

/// `bot.transcript.offbox` params. Cold — never wakes the box.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotTranscriptOffboxParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// `bot.transcript.offbox` result. `entries` is the upstream page, verbatim.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotTranscriptOffboxResult {
    pub entries: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── bot.subscribe / bot.unsubscribe ──────────────────────────────────────

/// `bot.subscribe` / `bot.unsubscribe` params.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotSubscribeParams {
    pub agent_ids: Vec<String>,
    /// Opt in to full-fidelity SSE payloads (inline avatars). Default slim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full_fidelity: bool,
}

/// `bot.unsubscribe` params — same shape as [`BotSubscribeParams`].
#[typeshare]
pub type BotUnsubscribeParams = BotSubscribeParams;

/// `bot.subscribe` result.
#[typeshare]
pub type BotSubscribeResult = BotEmptyResult;

/// `bot.unsubscribe` result.
#[typeshare]
pub type BotUnsubscribeResult = BotEmptyResult;

// ── bot.bindConversation ─────────────────────────────────────────────────

/// `bot.bindConversation` params. Binding is an index only; the hub never
/// infers a command target from `conversationId`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotBindConversationParams {
    pub conversation_id: String,
    pub agent_ids: Vec<String>,
    pub primary: String,
}

/// `bot.bindConversation` result.
#[typeshare]
pub type BotBindConversationResult = BotEmptyResult;

// ── Closed error enum ────────────────────────────────────────────────────

/// Closed hub-owned error code. This list is the client stability boundary.
///
/// Senders emit only these codes. Receivers treat any unknown wire string
/// as `upstream_error` and keep `retryable` / `detail`.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRelayErrorCode {
    IdentityUnavailable,
    NoPlan,
    UsageExhausted,
    BoxMigrating,
    BoxRecreating,
    BoxUnavailable,
    CommandRejected,
    ComputerUnavailable,
    UpstreamError,
}

impl BotRelayErrorCode {
    pub const ALL: &'static [Self] = &[
        Self::IdentityUnavailable,
        Self::NoPlan,
        Self::UsageExhausted,
        Self::BoxMigrating,
        Self::BoxRecreating,
        Self::BoxUnavailable,
        Self::CommandRejected,
        Self::ComputerUnavailable,
        Self::UpstreamError,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityUnavailable => "identity_unavailable",
            Self::NoPlan => "no_plan",
            Self::UsageExhausted => "usage_exhausted",
            Self::BoxMigrating => "box_migrating",
            Self::BoxRecreating => "box_recreating",
            Self::BoxUnavailable => "box_unavailable",
            Self::CommandRejected => "command_rejected",
            Self::ComputerUnavailable => "computer_unavailable",
            Self::UpstreamError => "upstream_error",
        }
    }

    /// Parse a wire code. Unknown strings become [`Self::UpstreamError`].
    pub fn from_wire(s: &str) -> Self {
        match s {
            "identity_unavailable" => Self::IdentityUnavailable,
            "no_plan" => Self::NoPlan,
            "usage_exhausted" => Self::UsageExhausted,
            "box_migrating" => Self::BoxMigrating,
            "box_recreating" => Self::BoxRecreating,
            "box_unavailable" => Self::BoxUnavailable,
            "command_rejected" => Self::CommandRejected,
            "computer_unavailable" => Self::ComputerUnavailable,
            _ => Self::UpstreamError,
        }
    }

    /// Closest existing [`crate::ERROR_CODES`] numeric. Receivers switch on
    /// the string [`Self::as_str`] in `data`, not this companion.
    pub const fn jsonrpc_numeric(self) -> i32 {
        match self {
            Self::NoPlan => -32003,         // forbidden
            Self::UsageExhausted => -32099, // rate_limited
            Self::IdentityUnavailable
            | Self::BoxMigrating
            | Self::BoxRecreating
            | Self::BoxUnavailable
            | Self::ComputerUnavailable => -32013, // tool_unavailable
            Self::CommandRejected => -32600, // invalid_request
            Self::UpstreamError => -32603,  // internal_error
        }
    }

    /// [`crate::ERROR_CODES`] key paired with [`Self::jsonrpc_numeric`].
    pub const fn jsonrpc_code_key(self) -> &'static str {
        match self {
            Self::NoPlan => "forbidden",
            Self::UsageExhausted => "rate_limited",
            Self::IdentityUnavailable
            | Self::BoxMigrating
            | Self::BoxRecreating
            | Self::BoxUnavailable
            | Self::ComputerUnavailable => "tool_unavailable",
            Self::CommandRejected => "invalid_request",
            Self::UpstreamError => "internal_error",
        }
    }
}

impl fmt::Display for BotRelayErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotRelayErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&s))
    }
}

/// Opaque upstream diagnostic. Present for debugging only; clients must
/// not parse `upstream`.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRelayErrorDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// Hub-owned bot-relay error object.
///
/// Wire form: `{code, retryable, detail, reason?}`.
/// `detail` is always present (empty object when unused).
/// `reason` is set only for [`BotRelayErrorCode::CommandRejected`].
///
/// On the JSON-RPC envelope this object is `error.data`. Receivers
/// switch on `data.code`. The envelope `error.message` is the snake_case
/// [`BotRelayErrorCode`].
///
/// `code` is a string on the wire. Generated clients see `string` and
/// compare against [`BotRelayErrorCode`]. Unknown values degrade to
/// `upstream_error` while preserving `retryable` and `detail`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotRelayError {
    #[typeshare(serialized_as = "String")]
    pub code: BotRelayErrorCode,
    pub retryable: bool,
    #[serde(default)]
    pub detail: BotRelayErrorDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&BotRelayError> for JsonRpcError {
    fn from(err: &BotRelayError) -> Self {
        let data = serde_json::to_value(err);
        debug_assert!(data.is_ok(), "BotRelayError must serialize");
        JsonRpcError {
            code: err.code.jsonrpc_numeric(),
            message: err.code.as_str().to_owned(),
            data: data.ok(),
        }
    }
}

impl From<BotRelayError> for JsonRpcError {
    fn from(err: BotRelayError) -> Self {
        JsonRpcError::from(&err)
    }
}

// ── Event channel ────────────────────────────────────────────────────────

/// Enumerated hub-owned event channel. Prefixed `hub:` so the set is
/// structurally disjoint from any unprefixed upstream channel name.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum HubChannel {
    #[serde(rename = "hub:turn_finished")]
    TurnFinished,
    #[serde(rename = "hub:resync_required")]
    ResyncRequired,
}

impl HubChannel {
    pub const ALL: &'static [Self] = &[Self::TurnFinished, Self::ResyncRequired];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnFinished => "hub:turn_finished",
            Self::ResyncRequired => "hub:resync_required",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "hub:turn_finished" => Some(Self::TurnFinished),
            "hub:resync_required" => Some(Self::ResyncRequired),
            _ => None,
        }
    }
}

impl fmt::Display for HubChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Future `hub:*` channel this crate does not yet name. Constructed only
/// via [`BotEventChannel::from_wire`]; the string includes the `hub:` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HubUnknownChannel(String);

impl HubUnknownChannel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Upstream-verbatim channel. Never starts with `hub:`. Constructed only
/// via [`BotEventChannel::from_wire`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamChannel(String);

impl UpstreamChannel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `bot.event` channel: an upstream-verbatim string or a hub-owned `hub:*`
/// name.
///
/// Known `hub:*` values are [`Self::Hub`]. Any other `hub:`-prefixed
/// string is [`Self::HubUnknown`]. Non-`hub:` strings are [`Self::Upstream`].
#[derive(Debug, Clone)]
pub enum BotEventChannel {
    Hub(HubChannel),
    HubUnknown(HubUnknownChannel),
    Upstream(UpstreamChannel),
}

impl BotEventChannel {
    pub fn from_wire(s: impl Into<String>) -> Self {
        let s = s.into();
        if let Some(hub) = HubChannel::from_wire(&s) {
            return Self::Hub(hub);
        }
        if s.starts_with("hub:") {
            return Self::HubUnknown(HubUnknownChannel(s));
        }
        Self::Upstream(UpstreamChannel(s))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Hub(hub) => hub.as_str(),
            Self::HubUnknown(s) => s.as_str(),
            Self::Upstream(s) => s.as_str(),
        }
    }
}

impl From<HubChannel> for BotEventChannel {
    fn from(hub: HubChannel) -> Self {
        Self::Hub(hub)
    }
}

impl PartialEq for BotEventChannel {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for BotEventChannel {}

impl Hash for BotEventChannel {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Display for BotEventChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for BotEventChannel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BotEventChannel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire(s))
    }
}

// ── Hub-owned event bodies ───────────────────────────────────────────────

/// Body of `hub:turn_finished` (`event` when [`HubChannel::TurnFinished`]).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubTurnFinishedEvent {
    pub agent_id: String,
    pub conversation_ids: Vec<String>,
    pub preview: String,
}

/// Body of `hub:resync_required` (`event` when [`HubChannel::ResyncRequired`]).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubResyncRequiredEvent {
    pub agent_id: String,
}

// ── bot.event envelope ───────────────────────────────────────────────────

/// `bot.event` envelope v1 (`v == `[`BOT_EVENT_ENVELOPE_V`]).
///
/// `seq` is a per-(connection, agent) monotonic counter starting at 1 on
/// each `bot.subscribe`. It is an ordering reference, not a dedupe key
/// (a redelivered event gets a fresh `seq`) and is never comparable
/// across connections. Resync is signaled by [`HubChannel::ResyncRequired`]
/// or a reconnect, never inferred from `seq`.
///
/// [`Self::event_id`] is reserved for content-identity dedupe and is
/// omitted from the wire when `None`.
///
/// `event` is upstream-verbatim for [`BotEventChannel::Upstream`]. For
/// [`HubChannel::TurnFinished`] it is [`HubTurnFinishedEvent`]; for
/// [`HubChannel::ResyncRequired`] it is [`HubResyncRequiredEvent`].
///
/// `channel` is a string on the wire. Typeshare cannot express the
/// hub / hub-unknown / upstream split, so generated clients see `string`.
/// Compare against [`HubChannel`] for the known `hub:*` values.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotEventEnvelope {
    pub v: u32,
    pub agent_id: String,
    // typeshare panics on bare 64-bit ints; `I54` is a JS-safe number.
    #[typeshare(serialized_as = "I54")]
    pub seq: u64,
    #[typeshare(serialized_as = "String")]
    pub channel: BotEventChannel,
    pub event: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl BotEventEnvelope {
    pub fn new(
        agent_id: impl Into<String>,
        seq: u64,
        channel: impl Into<BotEventChannel>,
        event: serde_json::Value,
    ) -> Self {
        Self {
            v: BOT_EVENT_ENVELOPE_V,
            agent_id: agent_id.into(),
            seq,
            channel: channel.into(),
            event,
            event_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_codes;
    use serde_json::{Value, json};

    fn roundtrip<T>(value: &T) -> Value
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).expect("serialize");
        let parsed: T = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(&parsed, value, "round-trip mismatch");
        json
    }

    fn assert_rejects<T>(value: Value)
    where
        T: serde::de::DeserializeOwned,
    {
        assert!(
            serde_json::from_value::<T>(value.clone()).is_err(),
            "expected deserialize error for {value}"
        );
    }

    #[test]
    fn capabilities_list_is_the_bot_verb_set() {
        let expected = [
            "bot.command",
            "bot.vncDescriptor",
            "bot.roster",
            "bot.status",
            "bot.transcript.offbox",
            "bot.subscribe",
            "bot.unsubscribe",
            "bot.bindConversation",
            "bot.event",
        ];
        assert_eq!(BOT_RELAY_CAPABILITIES, expected);
        for wire in expected {
            assert!(
                Method::from_wire_str(wire).is_some(),
                "capability {wire} must be a Method"
            );
        }
        assert!(!expected.contains(&"bot.ensureBox"));
        assert!(Method::from_wire_str("bot.ensureBox").is_none());
    }

    #[test]
    fn command_params_match_design_frame() {
        let params = BotCommandParams {
            agent_id: "agt_...".to_owned(),
            name: "sendPrompt".to_owned(),
            args: json!({"prompt": "hello"}),
        };
        let wire = json!({
            "agentId": "agt_...",
            "name": "sendPrompt",
            "args": {"prompt": "hello"},
        });
        assert_eq!(roundtrip(&params), wire);
        let parsed: BotCommandParams = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, params);
        assert_rejects::<BotCommandParams>(json!({
            "agent_id": "agt_...",
            "name": "sendPrompt",
            "args": {"prompt": "hello"},
        }));
    }

    #[test]
    fn command_args_accept_any_json_value() {
        for args in [json!(null), json!([]), json!("x")] {
            let params = BotCommandParams {
                agent_id: "agt_1".to_owned(),
                name: "noop".to_owned(),
                args: args.clone(),
            };
            assert_eq!(
                roundtrip(&params),
                json!({"agentId": "agt_1", "name": "noop", "args": args})
            );
        }
    }

    #[test]
    fn vnc_descriptor_result_null_expires_hint() {
        let result = BotVncDescriptorResult {
            vnc_url: "https://example.invalid/vnc".to_owned(),
            expires_hint: None,
        };
        let wire = json!({
            "vncUrl": "https://example.invalid/vnc",
            "expiresHint": null,
        });
        assert_eq!(roundtrip(&result), wire);
        let parsed: BotVncDescriptorResult = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, result);
        let omitted: BotVncDescriptorResult = serde_json::from_value(json!({
            "vncUrl": "https://example.invalid/vnc",
        }))
        .unwrap();
        assert_eq!(omitted.expires_hint, None);
        assert_rejects::<BotVncDescriptorResult>(json!({
            "vnc_url": "https://example.invalid/vnc",
            "expiresHint": null,
        }));
    }

    #[test]
    fn vnc_descriptor_result_with_expires_hint() {
        let result = BotVncDescriptorResult {
            vnc_url: "https://example.invalid/vnc".to_owned(),
            expires_hint: Some(1_700_000_000_000_i64),
        };
        assert_eq!(
            roundtrip(&result),
            json!({
                "vncUrl": "https://example.invalid/vnc",
                "expiresHint": 1_700_000_000_000_i64,
            })
        );
    }

    #[test]
    fn empty_params_serialize_as_object() {
        assert_eq!(roundtrip(&BotEmptyParams {}), json!({}));
        assert_eq!(roundtrip(&BotEmptyResult {}), json!({}));
    }

    #[test]
    fn roster_round_trip_camel_case() {
        let roster = BotRosterResult {
            agents: vec![BotRosterEntry {
                agent_id: "agt_1".to_owned(),
                name: "Watcher".to_owned(),
                status: "idle".to_owned(),
                last_turn_at: Some(1_700_000_123_000_i64),
            }],
        };
        let wire = json!({
            "agents": [{
                "agentId": "agt_1",
                "name": "Watcher",
                "status": "idle",
                "lastTurnAt": 1_700_000_123_000_i64,
            }],
        });
        assert_eq!(roundtrip(&roster), wire);
        let parsed: BotRosterResult = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, roster);
        assert_rejects::<BotRosterEntry>(json!({
            "agent_id": "agt_1",
            "name": "Watcher",
            "status": "idle",
        }));
    }

    #[test]
    fn empty_roster_is_agents_empty_array() {
        assert_eq!(
            roundtrip(&BotRosterResult::default()),
            json!({"agents": []})
        );
        let parsed: BotRosterResult = serde_json::from_value(json!({"agents": []})).unwrap();
        assert!(parsed.agents.is_empty());
    }

    #[test]
    fn last_turn_at_omitted_and_null_are_none() {
        let no_turn = BotRosterEntry {
            agent_id: "agt_2".to_owned(),
            name: "New".to_owned(),
            status: "unknown".to_owned(),
            last_turn_at: None,
        };
        assert!(
            !roundtrip(&no_turn)
                .as_object()
                .unwrap()
                .contains_key("lastTurnAt")
        );
        let omitted: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_2",
            "name": "New",
            "status": "unknown",
        }))
        .unwrap();
        assert_eq!(omitted.last_turn_at, None);
        let explicit_null: BotRosterEntry = serde_json::from_value(json!({
            "agentId": "agt_2",
            "name": "New",
            "status": "unknown",
            "lastTurnAt": null,
        }))
        .unwrap();
        assert_eq!(explicit_null.last_turn_at, None);
    }

    #[test]
    fn status_result_round_trip() {
        let status = BotStatusResult {
            run_state: BotRunState::Hibernated,
        };
        assert_eq!(roundtrip(&status), json!({"runState": "hibernated"}));
        let parsed: BotStatusResult =
            serde_json::from_value(json!({"runState": "hibernated"})).unwrap();
        assert_eq!(parsed, status);
        assert_rejects::<BotStatusResult>(json!({"run_state": "hibernated"}));
    }

    #[test]
    fn run_state_closed_send_open_receive() {
        for state in BotRunState::ALL {
            assert_eq!(BotRunState::from_wire(state.as_str()), *state);
            assert_eq!(roundtrip(state), json!(state.as_str()));
            assert_eq!(state.to_string(), state.as_str());
        }
        let unknown: BotRunState =
            serde_json::from_value(json!("draining")).expect("unknown run state");
        assert_eq!(unknown, BotRunState::Unknown);
        assert_eq!(serde_json::to_value(unknown).unwrap(), json!("unknown"));
    }

    #[test]
    fn transcript_offbox_params_and_result() {
        let params = BotTranscriptOffboxParams {
            agent_id: "agt_...".to_owned(),
            cursor: Some("c_1".to_owned()),
        };
        let wire = json!({"agentId": "agt_...", "cursor": "c_1"});
        assert_eq!(roundtrip(&params), wire);
        let parsed: BotTranscriptOffboxParams = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, params);

        let first_page = BotTranscriptOffboxParams {
            agent_id: "agt_...".to_owned(),
            cursor: None,
        };
        assert_eq!(roundtrip(&first_page), json!({"agentId": "agt_..."}));

        let result = BotTranscriptOffboxResult {
            entries: json!([{"id": "e1"}]),
            next_cursor: Some("c_2".to_owned()),
        };
        assert_eq!(
            roundtrip(&result),
            json!({"entries": [{"id": "e1"}], "nextCursor": "c_2"})
        );

        let last_page = BotTranscriptOffboxResult {
            entries: json!([{"id": "e9"}]),
            next_cursor: None,
        };
        let last_wire = json!({"entries": [{"id": "e9"}]});
        assert_eq!(roundtrip(&last_page), last_wire);
        let parsed: BotTranscriptOffboxResult = serde_json::from_value(last_wire).unwrap();
        assert_eq!(parsed.next_cursor, None);

        assert_rejects::<BotTranscriptOffboxParams>(json!({
            "agent_id": "agt_...",
            "cursor": "c_1",
        }));
    }

    #[test]
    fn subscribe_and_bind() {
        let sub = BotSubscribeParams {
            agent_ids: vec!["agt_a".to_owned(), "agt_b".to_owned()],
            full_fidelity: false,
        };
        assert_eq!(roundtrip(&sub), json!({"agentIds": ["agt_a", "agt_b"]}));

        let empty = BotSubscribeParams {
            agent_ids: vec![],
            full_fidelity: false,
        };
        let empty_wire = json!({"agentIds": []});
        assert_eq!(roundtrip(&empty), empty_wire);
        let parsed: BotSubscribeParams = serde_json::from_value(empty_wire).unwrap();
        assert!(parsed.agent_ids.is_empty());

        let bind = BotBindConversationParams {
            conversation_id: "conv_1".to_owned(),
            agent_ids: vec!["agt_a".to_owned()],
            primary: "agt_a".to_owned(),
        };
        let bind_wire = json!({
            "conversationId": "conv_1",
            "agentIds": ["agt_a"],
            "primary": "agt_a",
        });
        assert_eq!(roundtrip(&bind), bind_wire);
        let parsed: BotBindConversationParams = serde_json::from_value(bind_wire).unwrap();
        assert_eq!(parsed, bind);
        assert_rejects::<BotBindConversationParams>(json!({
            "conversation_id": "conv_1",
            "agent_ids": ["agt_a"],
            "primary": "agt_a",
        }));

        let empty_bind = BotBindConversationParams {
            conversation_id: "conv_2".to_owned(),
            agent_ids: vec![],
            primary: "agt_a".to_owned(),
        };
        let empty_bind_wire = json!({
            "conversationId": "conv_2",
            "agentIds": [],
            "primary": "agt_a",
        });
        assert_eq!(roundtrip(&empty_bind), empty_bind_wire);
        let parsed: BotBindConversationParams = serde_json::from_value(empty_bind_wire).unwrap();
        assert!(parsed.agent_ids.is_empty());
    }

    #[test]
    fn error_codes_round_trip_and_unknown_becomes_upstream_error() {
        for code in BotRelayErrorCode::ALL {
            assert_eq!(BotRelayErrorCode::from_wire(code.as_str()), *code);
            assert_eq!(roundtrip(code), json!(code.as_str()));
            assert_eq!(code.to_string(), code.as_str());
        }

        let unknown: BotRelayErrorCode =
            serde_json::from_value(json!("brand_new_code")).expect("unknown code must parse");
        assert_eq!(unknown, BotRelayErrorCode::UpstreamError);
        assert_eq!(
            serde_json::to_value(unknown).unwrap(),
            json!("upstream_error")
        );
    }

    #[test]
    fn retired_not_linked_degrades_to_upstream_error() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "not_linked",
            "retryable": false,
            "detail": {"upstream": "legacy client"},
        }))
        .expect("retired not_linked must deserialize");
        assert_eq!(parsed.code, BotRelayErrorCode::UpstreamError);
        assert!(!parsed.retryable);
        assert_eq!(parsed.detail.upstream.as_deref(), Some("legacy client"));
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {"upstream": "legacy client"},
            })
        );
    }

    #[test]
    fn error_object_matches_design_frame() {
        let err = BotRelayError {
            code: BotRelayErrorCode::UsageExhausted,
            retryable: false,
            detail: BotRelayErrorDetail {
                upstream: Some("...".to_owned()),
            },
            reason: None,
        };
        let wire = json!({
            "code": "usage_exhausted",
            "retryable": false,
            "detail": {"upstream": "..."},
        });
        assert_eq!(roundtrip(&err), wire);
        let parsed: BotRelayError = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, err);
    }

    #[test]
    fn identity_unavailable_is_retryable_and_omits_login_url() {
        let err = BotRelayError {
            code: BotRelayErrorCode::IdentityUnavailable,
            retryable: true,
            detail: BotRelayErrorDetail::default(),
            reason: None,
        };
        let wire = roundtrip(&err);
        assert_eq!(
            wire,
            json!({
                "code": "identity_unavailable",
                "retryable": true,
                "detail": {},
            })
        );
        assert!(!wire.as_object().unwrap().contains_key("loginUrl"));
        assert_eq!(err.code.jsonrpc_code_key(), "tool_unavailable");
        assert_eq!(err.code.jsonrpc_numeric(), -32013);
    }

    #[test]
    fn command_rejected_carries_reason() {
        let err = BotRelayError {
            code: BotRelayErrorCode::CommandRejected,
            retryable: false,
            detail: BotRelayErrorDetail::default(),
            reason: Some(COMMAND_REJECTED_NOT_YET_ENABLED.to_owned()),
        };
        assert_eq!(
            roundtrip(&err),
            json!({
                "code": "command_rejected",
                "retryable": false,
                "detail": {},
                "reason": "not_yet_enabled",
            })
        );
    }

    #[test]
    fn unknown_error_code_preserves_fields_and_reserializes_as_upstream_error() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "some_future_code",
            "retryable": true,
            "detail": {"upstream": "upstream rejected the request"},
            "loginUrl": "https://example.invalid/unused",
            "reason": "future_reason",
        }))
        .expect("unknown code must deserialize");
        assert_eq!(parsed.code, BotRelayErrorCode::UpstreamError);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": true,
                "detail": {"upstream": "upstream rejected the request"},
                "reason": "future_reason",
            })
        );
        assert!(
            !serde_json::to_value(&parsed)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("loginUrl")
        );

        let with_reason_only: BotRelayError = serde_json::from_value(json!({
            "code": "brand_new",
            "retryable": false,
            "reason": "x",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&with_reason_only).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {},
                "reason": "x",
            })
        );

        let no_optionals: BotRelayError = serde_json::from_value(json!({
            "code": "brand_new",
            "retryable": false,
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&no_optionals).unwrap(),
            json!({
                "code": "upstream_error",
                "retryable": false,
                "detail": {},
            })
        );
    }

    #[test]
    fn missing_detail_defaults() {
        let parsed: BotRelayError = serde_json::from_value(json!({
            "code": "box_migrating",
            "retryable": true,
        }))
        .expect("detail is optional on receive");
        assert_eq!(parsed.code, BotRelayErrorCode::BoxMigrating);
        assert!(parsed.retryable);
        assert_eq!(parsed.detail, BotRelayErrorDetail::default());
    }

    #[test]
    fn bot_relay_error_wraps_as_jsonrpc_data() {
        for code in BotRelayErrorCode::ALL {
            assert_eq!(
                code.jsonrpc_numeric(),
                error_codes::numeric_for(code.jsonrpc_code_key()).expect("ERROR_CODES key"),
                "{code}"
            );
            let err = BotRelayError {
                code: *code,
                retryable: false,
                detail: BotRelayErrorDetail::default(),
                reason: None,
            };
            let envelope: JsonRpcError = err.clone().into();
            assert_eq!(envelope.code, code.jsonrpc_numeric(), "{code}");
            assert_eq!(envelope.message, code.as_str(), "{code}");
            let data = envelope.data.expect("data");
            let back: BotRelayError = serde_json::from_value(data).unwrap();
            assert_eq!(back, err, "{code}");
        }
    }

    #[test]
    fn hub_channel_all_round_trips_and_unprefixed_stays_upstream() {
        for ch in HubChannel::ALL {
            assert_eq!(HubChannel::from_wire(ch.as_str()), Some(*ch));
            assert_eq!(roundtrip(&BotEventChannel::from(*ch)), json!(ch.as_str()));
            assert_eq!(ch.to_string(), ch.as_str());
            assert_eq!(BotEventChannel::from(*ch).to_string(), ch.as_str());
        }
        let unprefixed = BotEventChannel::from_wire("turn_finished");
        assert!(matches!(unprefixed, BotEventChannel::Upstream(_)));
        assert_eq!(unprefixed.as_str(), "turn_finished");
        assert_eq!(roundtrip(&unprefixed), json!("turn_finished"));
    }

    #[test]
    fn channel_upstream_and_hub_wire_forms() {
        assert_eq!(
            roundtrip(&BotEventChannel::from_wire("transcript")),
            json!("transcript")
        );
        assert_eq!(
            roundtrip(&BotEventChannel::from(HubChannel::TurnFinished)),
            json!("hub:turn_finished")
        );
        assert_eq!(
            roundtrip(&BotEventChannel::from(HubChannel::ResyncRequired)),
            json!("hub:resync_required")
        );

        let future: BotEventChannel =
            serde_json::from_value(json!("hub:future_channel")).expect("future hub:* must parse");
        assert!(matches!(future, BotEventChannel::HubUnknown(_)));
        assert_eq!(future.as_str(), "hub:future_channel");
        assert_eq!(future.to_string(), "hub:future_channel");
        assert_ne!(
            future,
            BotEventChannel::from_wire("transcript"),
            "hub-unknown is not an upstream channel"
        );
    }

    #[test]
    fn event_envelope_matches_design_frame() {
        let env = BotEventEnvelope {
            v: 1,
            agent_id: "agt_...".to_owned(),
            seq: 1088,
            channel: BotEventChannel::from_wire("transcript"),
            event: json!({"kind": "entry"}),
            event_id: None,
        };
        let wire = json!({
            "v": 1,
            "agentId": "agt_...",
            "seq": 1088,
            "channel": "transcript",
            "event": {"kind": "entry"},
        });
        assert_eq!(roundtrip(&env), wire);
        let parsed: BotEventEnvelope = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, env);
        assert!(
            !serde_json::to_value(&env)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("eventId")
        );
        assert_rejects::<BotEventEnvelope>(json!({
            "v": 1,
            "agent_id": "agt_...",
            "seq": 1088,
            "channel": "transcript",
            "event": {"kind": "entry"},
        }));
    }

    #[test]
    fn event_id_omitted_and_null_are_none() {
        let omitted: BotEventEnvelope = serde_json::from_value(json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 1,
            "channel": "transcript",
            "event": {},
        }))
        .unwrap();
        assert_eq!(omitted.event_id, None);
        let explicit_null: BotEventEnvelope = serde_json::from_value(json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 1,
            "channel": "transcript",
            "event": {},
            "eventId": null,
        }))
        .unwrap();
        assert_eq!(explicit_null.event_id, None);
    }

    #[test]
    fn event_envelope_with_event_id() {
        let env = BotEventEnvelope {
            v: BOT_EVENT_ENVELOPE_V,
            agent_id: "agt_1".to_owned(),
            seq: 1,
            channel: HubChannel::TurnFinished.into(),
            event: json!({"preview": "done"}),
            event_id: Some("evt_9".to_owned()),
        };
        assert_eq!(
            roundtrip(&env),
            json!({
                "v": 1,
                "agentId": "agt_1",
                "seq": 1,
                "channel": "hub:turn_finished",
                "event": {"preview": "done"},
                "eventId": "evt_9",
            })
        );
    }

    #[test]
    fn hub_owned_event_bodies_round_trip() {
        let finished = HubTurnFinishedEvent {
            agent_id: "agt_1".to_owned(),
            conversation_ids: vec!["conv_1".to_owned()],
            preview: "done".to_owned(),
        };
        assert_eq!(
            roundtrip(&finished),
            json!({
                "agentId": "agt_1",
                "conversationIds": ["conv_1"],
                "preview": "done",
            })
        );
        let resync = HubResyncRequiredEvent {
            agent_id: "agt_1".to_owned(),
        };
        assert_eq!(roundtrip(&resync), json!({"agentId": "agt_1"}));
        assert_rejects::<HubResyncRequiredEvent>(json!({"agent_id": "agt_1"}));
    }

    #[test]
    fn event_envelope_composes_hub_owned_bodies() {
        let finished = HubTurnFinishedEvent {
            agent_id: "agt_1".to_owned(),
            conversation_ids: vec!["conv_1".to_owned()],
            preview: "done".to_owned(),
        };
        let finished_env = BotEventEnvelope::new(
            "agt_1",
            2,
            HubChannel::TurnFinished,
            serde_json::to_value(&finished).unwrap(),
        );
        let finished_wire = json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 2,
            "channel": "hub:turn_finished",
            "event": {
                "agentId": "agt_1",
                "conversationIds": ["conv_1"],
                "preview": "done",
            },
        });
        assert_eq!(roundtrip(&finished_env), finished_wire);
        let parsed: BotEventEnvelope = serde_json::from_value(finished_wire).unwrap();
        assert_eq!(parsed.channel, HubChannel::TurnFinished.into());
        let parsed_body: HubTurnFinishedEvent = serde_json::from_value(parsed.event).unwrap();
        assert_eq!(parsed_body, finished);

        let resync = HubResyncRequiredEvent {
            agent_id: "agt_1".to_owned(),
        };
        let resync_env = BotEventEnvelope::new(
            "agt_1",
            3,
            HubChannel::ResyncRequired,
            serde_json::to_value(&resync).unwrap(),
        );
        let resync_wire = json!({
            "v": 1,
            "agentId": "agt_1",
            "seq": 3,
            "channel": "hub:resync_required",
            "event": {"agentId": "agt_1"},
        });
        assert_eq!(roundtrip(&resync_env), resync_wire);
        let parsed: BotEventEnvelope = serde_json::from_value(resync_wire).unwrap();
        assert_eq!(parsed.channel, HubChannel::ResyncRequired.into());
        let parsed_body: HubResyncRequiredEvent = serde_json::from_value(parsed.event).unwrap();
        assert_eq!(parsed_body, resync);
    }

    #[test]
    fn hello_ack_can_advertise_bot_verbs() {
        use crate::{ConnectionId, HelloAckMsg, UserId};

        let ack = HelloAckMsg {
            connection_id: ConnectionId::new("conn_1").unwrap(),
            user_id: UserId::new("user_1").unwrap(),
            computer_hub_version: "0.1.0".to_owned(),
            supported_protocol_versions: vec![crate::PROTOCOL_VERSION.to_owned()],
            capabilities: BOT_RELAY_CAPABILITIES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        };
        let json = serde_json::to_value(&ack).expect("serialize");
        let parsed: HelloAckMsg = serde_json::from_value(json).expect("deserialize");
        assert_eq!(
            parsed
                .capabilities
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            BOT_RELAY_CAPABILITIES
        );
    }

    #[test]
    fn event_envelope_new_omits_event_id_on_the_wire() {
        let env = BotEventEnvelope::new("agt_1", 1, HubChannel::ResyncRequired, json!({}));
        let wire = serde_json::to_value(&env).unwrap();
        assert_eq!(
            wire,
            json!({
                "v": 1,
                "agentId": "agt_1",
                "seq": 1,
                "channel": "hub:resync_required",
                "event": {},
            })
        );
        assert!(!wire.as_object().unwrap().contains_key("eventId"));
    }
}
