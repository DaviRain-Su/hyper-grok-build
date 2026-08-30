//! Wire names of the hub-synthesized Grok Bot harness tools.
//!
//! Single source of truth shared by the hub that registers the tools and
//! by agent hosts that gate the tools per agent config.

/// Handwritten v1 harness tool ids. The hub's registration specs are
/// tested against this list so the surface cannot drift silently.
pub const GROK_BOT_TOOL_IDS: &[&str] = &[
    "bot_create_agent",
    "bot_list_agents",
    "bot_send_prompt",
    "bot_get_agent_transcript",
    "bot_get_agent_transcript_page",
    "bot_get_agent_transcript_tail",
    "bot_get_agent_transcript_window",
    "bot_transcript_offbox",
    "bot_await_turn",
];

/// Whether `name` is a hub-synthesized Grok Bot harness tool.
pub fn is_grok_bot_tool(name: &str) -> bool {
    GROK_BOT_TOOL_IDS.contains(&name)
}

/// Model-facing descriptions, one per [`GROK_BOT_TOOL_IDS`] entry (same
/// order). Agent hosts use these to advertise opted-in bot tools before the
/// hub connection is live; the hub's registration specs are tested against
/// this table so the two surfaces cannot drift.
pub const GROK_BOT_TOOL_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "bot_create_agent",
        "Create a Grok Bot agent on the user's box with a short name and an \
         optional persona/description. Returns the new agent's id so you can \
         send to it with bot_send_prompt. Use this to spin up a focused \
         teammate for a job. There is no tool to delete an agent, so only \
         create one when it is genuinely useful; list existing agents with \
         bot_list_agents first.",
    ),
    (
        "bot_list_agents",
        "List Grok Bot agents on the user's box. Returns each agent's id \
         (and name / activity flags). Use this to get an agent_id for \
         bot_send_prompt, bot_await_turn, or a transcript tool. Takes no \
         arguments. Prefer listing before creating a duplicate with \
         bot_create_agent. This command wakes the box.",
    ),
    (
        "bot_send_prompt",
        "Send a prompt to a Grok Bot agent. Use this to deliver user text \
         (and optional workspace files via paths) to an existing agent_id \
         from bot_list_agents or bot_create_agent. mode (default \
         fire_and_forget) is fire_and_forget | blocking | async. \
         fire_and_forget returns {accepted} immediately. blocking waits and \
         returns the assistant text (timeout_ms is server-clamped; timeout \
         is finished:false, not an error). async returns {accepted, handle} \
         immediately and wakes the model when the turn ends (async may be \
         unavailable during rollout; on refusal use blocking or \
         fire_and_forget). Empty finished:true text means the turn ended \
         (interrupted, tools-only, or a non-text reply), not an error; \
         inspect a non-text reply with bot_get_agent_transcript_tail. On \
         timeout or failure, call bot_await_turn with the returned handle \
         — do not re-send the prompt. paths requires a bound workspace that \
         serves workspace.client_fs_read_file; omit or pass [] for no files. \
         Attachments are refused for live streaming agents. Waiting \
         (blocking/async) is not supported for live streaming agents or \
         agents that cannot stream live turns — use fire_and_forget and \
         read the transcript (bot_get_agent_transcript_tail after a send \
         that woke the box; bot_transcript_offbox when live waiting is \
         unsupported). Do not send another prompt while a turn is in \
         progress.",
    ),
    (
        "bot_get_agent_transcript",
        "Read a Grok Bot agent's full transcript from the box. Use this when \
         you need the entire conversation, not just recent entries. This \
         command wakes a hibernated box. Prefer bot_get_agent_transcript_tail \
         for the latest page, bot_get_agent_transcript_page for a \
         time-bounded retained page, or bot_transcript_offbox when you must \
         not wake the box (including agents that cannot stream live turns). Do not use \
         this to wait for a reply after bot_send_prompt — call \
         bot_await_turn with the returned handle; do not re-send the prompt.",
    ),
    (
        "bot_get_agent_transcript_page",
        "Read one time-bounded page of a Grok Bot agent's retained transcript \
         from the box. Requires limit and until_ms (inclusive unix-epoch \
         milliseconds). Optional since_ms is an inclusive lower bound; \
         optional before_seq is an exclusive sequence cursor from a previous \
         page. This command wakes a hibernated box. Use this to page older \
         retained history. Prefer tail for the latest entries, or \
         bot_transcript_offbox when you must not wake the box. Do not use \
         this to wait for an in-flight turn — call bot_await_turn; do not \
         re-send the prompt.",
    ),
    (
        "bot_get_agent_transcript_tail",
        "Read the latest page of a Grok Bot agent's transcript from the box. \
         Requires limit. Pass before_seq from a previous page to walk older \
         entries. Use this as the covering read after a fire-and-forget send \
         or a finished turn. This command wakes a hibernated box. Prefer \
         bot_transcript_offbox when you must not wake the box, including \
         agents that cannot stream live turns. Do not poll this while a handle is \
         outstanding — call bot_await_turn instead of re-sending the prompt.",
    ),
    (
        "bot_get_agent_transcript_window",
        "Read a Grok Bot agent's transcript from the box, including \
         per-thread counts (threadCounts) when present. Shares tail's \
         limit / before_seq pagination (omit before_seq for the latest \
         page). This command wakes a hibernated box. Use this when the \
         reply needs per-thread counts; prefer \
         bot_get_agent_transcript_tail as the covering latest-page read, \
         or bot_transcript_offbox when you must not wake the box. Do not \
         use this to wait for an in-flight turn — call bot_await_turn; do \
         not re-send the prompt.",
    ),
    (
        "bot_transcript_offbox",
        "Read a Grok Bot agent's transcript off-box. Never wakes the \
         box. Use this for cold hydration, for agents that cannot stream \
         live turns (waiting and live box events are unsupported), or whenever you must \
         not wake the box. Pass cursor from a previous off-box page to \
         continue; omit it on the first page. Prefer this over the box \
         transcript tools when waking the box would be wasteful. This does \
         not wait for an in-flight turn — after bot_send_prompt, use \
         bot_await_turn with the handle; do not re-send the prompt.",
    ),
    (
        "bot_await_turn",
        "Re-await a Grok Bot turn after a timeout, a lost notification, or \
         pod death. Pass the handle from bot_send_prompt unchanged. Without \
         a handle, waits for the agent to go idle and returns the last \
         send-message (agent-level). Do not send another prompt; that would \
         interrupt the turn. timeout_ms is server-clamped; timeout is \
         finished:false, not an error. Not supported for live streaming \
         agents or agents that cannot stream live turns — use \
         fire_and_forget and read the transcript (bot_transcript_offbox \
         when live waiting is unsupported). Use this instead \
         of re-sending the same prompt.",
    ),
];

/// The model-facing description for a Grok Bot tool id, if known.
pub fn grok_bot_tool_description(name: &str) -> Option<&'static str> {
    GROK_BOT_TOOL_DESCRIPTIONS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, desc)| *desc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_cover_every_id_in_order() {
        let desc_ids: Vec<&str> = GROK_BOT_TOOL_DESCRIPTIONS
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(desc_ids, GROK_BOT_TOOL_IDS);
        assert!(
            GROK_BOT_TOOL_DESCRIPTIONS
                .iter()
                .all(|(_, desc)| !desc.trim().is_empty())
        );
    }
}
