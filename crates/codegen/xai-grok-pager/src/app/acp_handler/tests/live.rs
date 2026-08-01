use super::*;
use crate::live::LiveCommand;
use crate::live::broker::LiveDelegationBroker;
use crate::live::state::{DraftSnapshot, LiveState};

#[test]
fn deduped_live_chunk_is_not_forwarded_twice_to_delegation_broker() {
    let session_id = "sess-live";
    let prompt_id = "pid-live";
    let delegation_id = "del-live";
    let mut app = make_app_with_agent(session_id);
    let agent_id = AgentId(0);

    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        agent.session.current_prompt_id = Some(prompt_id.to_string());
        agent.session.state = AgentState::TurnRunning;
    }

    let generation = app.live_runtime.next_generation();
    let mut broker = LiveDelegationBroker::new(generation);
    broker.bind(session_id.to_string(), agent_id);
    assert!(broker.register_delegation(delegation_id.to_string()));
    app.live_runtime.broker = broker;
    app.live_runtime.register_delegation(
        generation,
        delegation_id.to_string(),
        agent_id,
        session_id.to_string(),
        prompt_id.to_string(),
    );
    app.live_runtime.state = LiveState::Active {
        agent_id,
        session_id: session_id.to_string(),
        generation,
        draft: DraftSnapshot::default(),
    };
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(8);
    app.live_runtime.cmd_tx = Some(cmd_tx);

    // The second frame has the same eventId. The ACP highwater drops it from
    // rendering; the Live bridge must honor the same decision rather than
    // appending duplicate text to the delegation accumulator.
    assert!(handle(
        make_agent_chunk_with_event(session_id, "only once", prompt_id, Some("sess-live-1"),),
        &mut app,
    ));
    assert!(!handle(
        make_agent_chunk_with_event(session_id, "only once", prompt_id, Some("sess-live-1"),),
        &mut app,
    ));

    crate::live::acp_bridge::on_turn_completed(&mut app, session_id, prompt_id);
    let command = cmd_rx.try_recv().expect("terminal Live command");
    let LiveCommand::CompleteDelegation {
        delegation_id: actual_id,
        text,
    } = command
    else {
        panic!("expected CompleteDelegation");
    };
    assert_eq!(actual_id, delegation_id);
    assert!(text.contains("only once"));
    assert_eq!(text.matches("only once").count(), 1);
    assert!(
        cmd_rx.try_recv().is_err(),
        "terminal command is exactly once"
    );
}
