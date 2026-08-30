use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::Client as _;
use futures::stream::{FuturesUnordered, StreamExt as _};
use serde_json::value::RawValue;
use xai_grok_hooks::event::{HookEventEnvelope, HookEventName, HookPayload};
use xai_grok_telemetry::events::{ClientHookGateOutcome, HookBlockCause};

use super::{SessionActor, ToolLoop};
use crate::extensions::hooks::{
    ClientHookDecision, ClientHookDispatch, ClientHookGroup, ClientHookResponse,
};
use crate::sampling::types::ToolCallResponse;

const HOOK_EVENT_METHOD: &str = "x.ai/hooks/event";
const HOOK_RUN_METHOD: &str = "x.ai/hooks/run";

const CLIENT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

const CLIENT_STOP_GATE_TIMEOUT: Duration = Duration::from_secs(600);

pub(super) enum RewriteProblem {
    FailsSchema(String),
    RetargetsCall,
}

impl std::fmt::Display for RewriteProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailsSchema(error) => {
                write!(f, "the updatedInput failed the tool's schema: {error}")
            }
            Self::RetargetsCall => write!(f, "the updatedInput may not change which tool runs"),
        }
    }
}

enum ReverseOutcome {
    Responded(Arc<RawValue>),
    Transport(acp::Error),
    Timeout,
}

fn classify(outcome: ReverseOutcome) -> (ClientHookResponse, ClientHookGateOutcome) {
    let raw = match outcome {
        ReverseOutcome::Responded(raw) => raw,
        ReverseOutcome::Transport(err) => {
            tracing::warn!(%err, "x.ai/hooks/run transport error (no client wired?); failing open");
            return fail_open(ClientHookGateOutcome::TransportError);
        }
        ReverseOutcome::Timeout => {
            tracing::warn!("x.ai/hooks/run timed out; failing open");
            return fail_open(ClientHookGateOutcome::TimedOut);
        }
    };
    match serde_json::from_str::<ClientHookResponse>(raw.get()) {
        Ok(resp) => {
            let label = decision_label(resp.decision);
            (resp, label)
        }
        Err(err) => {
            tracing::warn!(%err, "malformed x.ai/hooks/run response; failing open");
            fail_open(ClientHookGateOutcome::Malformed)
        }
    }
}

fn fail_open(outcome: ClientHookGateOutcome) -> (ClientHookResponse, ClientHookGateOutcome) {
    (ClientHookResponse::default(), outcome)
}

fn decision_label(decision: ClientHookDecision) -> ClientHookGateOutcome {
    match decision {
        ClientHookDecision::Deny => ClientHookGateOutcome::Denied,
        ClientHookDecision::Continue => ClientHookGateOutcome::Proceeded,
        ClientHookDecision::Ask => {
            tracing::warn!(
                "x.ai/hooks/run returned 'ask'; client hooks cannot ask yet — failing open"
            );
            ClientHookGateOutcome::UnknownDecision
        }
        ClientHookDecision::Other => {
            tracing::warn!("x.ai/hooks/run returned an unknown decision value; failing open");
            ClientHookGateOutcome::UnknownDecision
        }
    }
}

fn matching_callback_ids<'a>(
    groups: &'a [ClientHookGroup],
    match_value: Option<&str>,
) -> Vec<&'a str> {
    groups
        .iter()
        .filter(|group| {
            xai_grok_hooks::matcher::matcher_allows(group.matcher.as_ref(), match_value)
        })
        .flat_map(|group| group.callback_ids.iter().map(String::as_str))
        .collect()
}

fn dispatch_params(dispatch: &ClientHookDispatch<'_>) -> Option<Arc<RawValue>> {
    serde_json::value::to_raw_value(dispatch)
        .inspect_err(|err| tracing::warn!(%err, "failed to serialize client hook dispatch"))
        .ok()
        .map(Into::into)
}

impl SessionActor {
    pub(super) fn make_hook_envelope(
        &self,
        hook_event_name: HookEventName,
        prompt_id: Option<String>,
        payload: HookPayload,
    ) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: hook_event_name.canonical(),
            session_id: self.session_id_string(),
            cwd: self.session_info.cwd.clone(),
            workspace_root: self.hook_workspace_root(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            transcript_path: self.get_transcript_path(),
            client_identifier: None,
            prompt_id,
            permission_mode: Some(self.permission_mode_label().to_string()),
            payload,
        }
    }

    pub(super) fn may_have_hooks_for(&self, event: HookEventName) -> bool {
        self.hook_registry.borrow().is_some()
            || self.client_hooks.borrow().contains_key(&event.canonical())
    }

    pub(super) fn has_enabled_hooks_for(&self, event: HookEventName) -> bool {
        self.hook_registry
            .borrow()
            .as_ref()
            .is_some_and(|registry| registry.has_enabled_hooks_for_canonical(event))
            || self.client_hooks.borrow().contains_key(&event.canonical())
    }

    pub(super) fn fire_hook(
        &self,
        hook_event_name: HookEventName,
        prompt_id: Option<String>,
        payload: HookPayload,
    ) -> HookEventEnvelope {
        let envelope = self.make_hook_envelope(hook_event_name, prompt_id, payload);
        self.notify_client_hooks(&envelope);
        envelope
    }

    pub(super) async fn deny_tool(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        tool_name: &str,
        hook_name: String,
        reason: String,
    ) -> Result<ToolLoop, acp::Error> {
        tracing::info!(%tool_name, %hook_name, %reason, "tool call denied by pre_tool_use hook");
        self.block_tool_call(
            model_call_id,
            tool_call_id,
            tool_name,
            hook_name,
            HookBlockCause::Denied,
            &reason,
        )
        .await
    }

    pub(super) async fn block_unusable_rewrite(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        tool_name: &str,
        hook_name: String,
        problem: RewriteProblem,
    ) -> Result<ToolLoop, acp::Error> {
        tracing::warn!(
            %tool_name,
            %hook_name,
            hook_failure = %problem,
            "pre_tool_use hook returned an unusable updatedInput"
        );
        self.block_tool_call(
            model_call_id,
            tool_call_id,
            tool_name,
            hook_name,
            HookBlockCause::UnusableRewrite,
            &problem.to_string(),
        )
        .await
    }

    async fn block_tool_call(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        tool_name: &str,
        hook_name: String,
        cause: HookBlockCause,
        detail: &str,
    ) -> Result<ToolLoop, acp::Error> {
        xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::HookBlocked {
            hook_name: hook_name.clone(),
            cause,
        });
        self.handle_tool_not_executed(
            model_call_id,
            tool_call_id,
            format!("Hook denied: {detail}"),
        )
        .await?;
        self.send_hook_annotation(&format!(
            "\u{26a0} `{tool_name}` blocked by hook `{hook_name}`: {detail}"
        ))
        .await;
        Ok(ToolLoop::HookDenied { hook_name })
    }

    fn client_gate_responses<'a>(
        &'a self,
        groups: &'a [ClientHookGroup],
        tool_name: Option<&'a str>,
        envelope: &'a HookEventEnvelope,
    ) -> FuturesUnordered<
        impl Future<Output = (&'a str, ClientHookResponse, Duration, ClientHookGateOutcome)> + 'a,
    > {
        let default_timeout =
            if envelope.hook_event_name.traits().gate == xai_grok_hooks::event::GateKind::Stop {
                CLIENT_STOP_GATE_TIMEOUT
            } else {
                CLIENT_HOOK_TIMEOUT
            };
        let mut seen = std::collections::HashSet::new();
        groups
            .iter()
            .filter(move |group| {
                xai_grok_hooks::matcher::matcher_allows(group.matcher.as_ref(), tool_name)
            })
            .flat_map(move |group| {
                let timeout = group.timeout.unwrap_or(default_timeout);
                group
                    .callback_ids
                    .iter()
                    .map(move |callback_id| (callback_id.as_str(), timeout))
            })
            .filter(move |(callback_id, _)| seen.insert(*callback_id))
            .map(move |(callback_id, timeout)| {
                let dispatch = ClientHookDispatch {
                    hook_callback_id: callback_id,
                    envelope,
                };
                async move {
                    let started = tokio::time::Instant::now();
                    let (response, gate_outcome) =
                        classify(self.send_hook_run(&dispatch, timeout).await);
                    let elapsed = started.elapsed();
                    xai_grok_telemetry::session_ctx::log_event(
                        xai_grok_telemetry::events::ClientHookGate {
                            callback_id: callback_id.to_string(),
                            tool_name: tool_name.map(str::to_string),
                            outcome: gate_outcome,
                            duration_ms: elapsed.as_millis() as u64,
                        },
                    );
                    (callback_id, response, elapsed, gate_outcome)
                }
            })
            .collect()
    }

    pub(super) async fn run_pre_tool_use_client_hook(
        &self,
        call: &ToolCallResponse,
        tool_call_id: &acp::ToolCallId,
        envelope: &HookEventEnvelope,
    ) -> Result<Option<ToolLoop>, acp::Error> {
        let Some(groups) = self
            .client_hooks
            .borrow()
            .get(&HookEventName::PreToolUse)
            .cloned()
        else {
            return Ok(None);
        };
        let tool_name = envelope
            .payload
            .match_value()
            .unwrap_or(call.function.name.as_str());

        let mut pending = self.client_gate_responses(&groups, Some(tool_name), envelope);
        while let Some((callback_id, response, _elapsed, _outcome)) = pending.next().await {
            if response.decision == ClientHookDecision::Deny {
                let reason = response
                    .system_message
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "blocked by client hook".to_string());
                return Ok(Some(
                    self.deny_tool(
                        &call.id,
                        tool_call_id,
                        tool_name,
                        format!("client:{callback_id}"),
                        reason,
                    )
                    .await?,
                ));
            }
        }
        Ok(None)
    }

    pub(super) async fn run_stop_client_hooks(
        &self,
        envelope: &HookEventEnvelope,
    ) -> xai_grok_hooks::dispatcher::StopDispatchResult {
        use xai_grok_hooks::result::HookRunResult;

        let mut out = xai_grok_hooks::dispatcher::StopDispatchResult::default();
        let Some(groups) = self
            .client_hooks
            .borrow()
            .get(&envelope.hook_event_name.canonical())
            .cloned()
        else {
            return out;
        };

        let match_value = envelope.payload.match_value();
        let mut pending = self.client_gate_responses(&groups, match_value, envelope);
        let mut responses = std::collections::HashMap::new();
        while let Some((callback_id, response, elapsed, gate_outcome)) = pending.next().await {
            responses.insert(callback_id, (response, elapsed, gate_outcome));
        }
        let ordered = groups
            .iter()
            .flat_map(|group| group.callback_ids.iter())
            .filter_map(|id| responses.remove(id.as_str()).map(|r| (id.as_str(), r)));
        for (callback_id, (response, elapsed, gate_outcome)) in ordered {
            let hook_name = format!("client:{callback_id}");
            let block_reason = (response.decision == ClientHookDecision::Deny).then(|| {
                response
                    .system_message
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "blocked by client hook".to_string())
            });
            let stop_reason = (response.continue_ == Some(false)).then(|| {
                response
                    .stop_reason
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "stopped by client hook".to_string())
            });

            let detail = xai_grok_hooks::dispatcher::stop_detail(
                stop_reason.is_some(),
                stop_reason.as_deref(),
                block_reason.as_deref(),
            );
            let unanswered = match gate_outcome {
                ClientHookGateOutcome::TimedOut => Some("timed out"),
                ClientHookGateOutcome::TransportError => Some("transport error"),
                ClientHookGateOutcome::Denied
                | ClientHookGateOutcome::Proceeded
                | ClientHookGateOutcome::Malformed
                | ClientHookGateOutcome::UnknownDecision => None,
            };
            out.results.push(match (detail, unanswered) {
                (Some(detail), _) => HookRunResult::Blocked {
                    hook_name: hook_name.clone(),
                    detail,
                    elapsed,
                    http_info: None,
                    system_message: None,
                },
                (None, Some(why)) => HookRunResult::Failed {
                    hook_name: hook_name.clone(),
                    error: format!("client hook {why}"),
                    elapsed,
                    http_info: None,
                    system_message: None,
                },
                (None, None) => HookRunResult::Success {
                    hook_name: hook_name.clone(),
                    elapsed,
                    http_info: None,
                    system_message: None,
                },
            });

            out.absorb(
                &hook_name,
                xai_grok_hooks::dispatcher::StopSignals {
                    block_reason,
                    stop_reason,
                    additional_context: response
                        .additional_context
                        .filter(|c| !c.trim().is_empty()),
                },
            );
        }
        out
    }

    async fn send_hook_run(
        &self,
        dispatch: &ClientHookDispatch<'_>,
        timeout: Duration,
    ) -> ReverseOutcome {
        let Some(params) = dispatch_params(dispatch) else {
            return ReverseOutcome::Transport(acp::Error::internal_error());
        };
        let ext_request = acp::ExtRequest::new(HOOK_RUN_METHOD, params);
        match tokio::time::timeout(timeout, self.notifications.gateway.ext_method(ext_request))
            .await
        {
            Ok(Ok(raw)) => ReverseOutcome::Responded(raw.0),
            Ok(Err(err)) => ReverseOutcome::Transport(err),
            Err(_) => ReverseOutcome::Timeout,
        }
    }

    pub(super) fn notify_client_hooks(&self, envelope: &HookEventEnvelope) {
        let hooks = self.client_hooks.borrow();
        let Some(groups) = hooks.get(&envelope.hook_event_name.canonical()) else {
            return;
        };
        let match_value = envelope.payload.match_value();
        for callback_id in matching_callback_ids(groups, match_value) {
            let dispatch = ClientHookDispatch {
                hook_callback_id: callback_id,
                envelope,
            };
            if let Some(params) = dispatch_params(&dispatch) {
                self.notifications
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(HOOK_EVENT_METHOD, params));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(value: serde_json::Value) -> Arc<RawValue> {
        serde_json::value::to_raw_value(&value).unwrap().into()
    }

    #[test]
    fn classify_only_deny_blocks_and_every_failure_mode_reports_a_distinct_outcome() {
        let (denied, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": "deny" }),
        )));
        assert_eq!(denied.decision, ClientHookDecision::Deny);
        assert!(matches!(outcome, ClientHookGateOutcome::Denied));

        let (cont, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": "continue" }),
        )));
        assert_eq!(cont.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::Proceeded));

        for decision in ["maybe_later", "ask"] {
            let (unknown, outcome) = classify(ReverseOutcome::Responded(raw(
                serde_json::json!({ "decision": decision }),
            )));
            assert_ne!(unknown.decision, ClientHookDecision::Deny, "{decision}");
            assert!(
                matches!(outcome, ClientHookGateOutcome::UnknownDecision),
                "{decision}"
            );
        }

        let (malformed, outcome) = classify(ReverseOutcome::Responded(raw(
            serde_json::json!({ "decision": 123 }),
        )));
        assert_eq!(malformed.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::Malformed));

        let (transport, outcome) =
            classify(ReverseOutcome::Transport(acp::Error::internal_error()));
        assert_eq!(transport.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::TransportError));

        let (timeout, outcome) = classify(ReverseOutcome::Timeout);
        assert_eq!(timeout.decision, ClientHookDecision::Continue);
        assert!(matches!(outcome, ClientHookGateOutcome::TimedOut));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn may_have_hooks_for_inert_vs_active() {
        use xai_grok_hooks::event::HookEventName;

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let actor = crate::session::acp_session::support::create_test_actor(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;

                assert!(actor.hook_registry.borrow().is_none());
                assert!(actor.client_hooks.borrow().is_empty());
                assert!(!actor.may_have_hooks_for(HookEventName::PreToolUse));

                actor.client_hooks.borrow_mut().insert(
                    HookEventName::PreToolUse,
                    vec![ClientHookGroup {
                        matcher: None,
                        callback_ids: vec!["cb_0".to_string()],
                        timeout: None,
                    }],
                );
                assert!(actor.may_have_hooks_for(HookEventName::PreToolUse));
                assert!(!actor.may_have_hooks_for(HookEventName::Stop));

                *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(
                    xai_grok_hooks::discovery::HookRegistry::default(),
                ));
                assert!(actor.may_have_hooks_for(HookEventName::Stop));
                assert!(actor.may_have_hooks_for(HookEventName::PostCompact));
            })
            .await;
    }

    #[test]
    fn matching_callback_ids_filters_by_matcher() {
        use xai_grok_hooks::matcher::HookMatcher;

        let groups = vec![
            ClientHookGroup {
                matcher: Some(HookMatcher::new("run_terminal_command").unwrap()),
                callback_ids: vec!["bash_only".to_string()],
                timeout: None,
            },
            ClientHookGroup {
                matcher: None,
                callback_ids: vec!["all_a".to_string(), "all_b".to_string()],
                timeout: None,
            },
            ClientHookGroup {
                matcher: Some(HookMatcher::new("read_file").unwrap()),
                callback_ids: vec!["read_only".to_string()],
                timeout: None,
            },
        ];

        assert_eq!(
            matching_callback_ids(&groups, Some("run_terminal_command")),
            ["bash_only", "all_a", "all_b"]
        );
        assert_eq!(
            matching_callback_ids(&groups, Some("list_dir")),
            ["all_a", "all_b"]
        );
        assert_eq!(
            matching_callback_ids(&groups, None),
            ["bash_only", "all_a", "all_b", "read_only"]
        );
    }
}
