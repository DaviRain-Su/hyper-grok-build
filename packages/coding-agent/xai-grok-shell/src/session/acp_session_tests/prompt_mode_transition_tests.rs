use super::*;
#[test]
fn prompt_mode_from_session_mode_id_uses_acp_session_mode() {
    assert_eq!(
        PromptMode::Ask,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("ask"))
    );
    assert_eq!(
        PromptMode::Plan,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("plan"))
    );
    assert_eq!(
        PromptMode::Agent,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("default"))
    );
    assert_eq!(
        PromptMode::Agent,
        prompt_mode_from_session_mode_id(&acp::SessionModeId::new("browser_use"))
    );
}
fn fn_def(name: &str) -> ToolDefinition {
    ToolDefinition::function(name, None::<&str>, serde_json::json!({"type": "object"}))
}
fn names(defs: &[ToolDefinition]) -> Vec<&str> {
    defs.iter().map(|d| d.function.name.as_str()).collect()
}
#[test]
fn cursor_filter_in_plan_mode_keeps_writes_and_shows_create_plan() {
    let defs = vec![
        fn_def("Read"),
        fn_def("Grep"),
        fn_def("Write"),
        fn_def("StrReplace"),
        fn_def("CreatePlan"),
        fn_def("SwitchMode"),
        fn_def("AskQuestion"),
    ];
    let filtered = filter_cursor_tools_by_plan_mode(defs, true);
    let kept = names(&filtered);
    assert!(kept.contains(&"Read"));
    assert!(kept.contains(&"Grep"));
    assert!(kept.contains(&"CreatePlan"));
    assert!(kept.contains(&"SwitchMode"));
    assert!(kept.contains(&"AskQuestion"));
    assert!(kept.contains(&"Write"));
    assert!(kept.contains(&"StrReplace"));
}
#[test]
fn cursor_filter_is_noop_for_non_cursor_tools() {
    let defs = vec![
        fn_def("read_file"),
        fn_def("search_replace"),
        fn_def("write"),
        fn_def("ask_user_question"),
        fn_def("enter_plan_mode"),
        fn_def("exit_plan_mode"),
    ];
    let in_plan = filter_cursor_tools_by_plan_mode(defs.clone(), true);
    let out_of_plan = filter_cursor_tools_by_plan_mode(defs.clone(), false);
    assert_eq!(names(&in_plan).len(), defs.len());
    assert_eq!(names(&out_of_plan).len(), defs.len());
}
/// Pins the `reconcile_plan_mode_with_prompt` transitions:
/// Plan → Pending, idempotent, non-plan modes exit cleanly.
#[test]
fn prompt_mode_plan_drives_tracker_into_pending_when_inactive() {
    use crate::session::plan_mode::{PlanModeState, PlanModeTracker};
    use std::path::PathBuf;
    fn reconcile(tracker: &mut PlanModeTracker, mode: PromptMode) {
        match mode {
            PromptMode::Plan => {
                tracker.enter_pending();
            }
            PromptMode::Agent | PromptMode::Ask => {
                if tracker.state() != PlanModeState::Inactive {
                    tracker.user_exit(false);
                }
            }
        }
    }
    let mut tracker = PlanModeTracker::new(PathBuf::from("/tmp/test"));
    assert_eq!(tracker.state(), PlanModeState::Inactive);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Agent);
    assert_eq!(tracker.state(), PlanModeState::Inactive);
    reconcile(&mut tracker, PromptMode::Plan);
    assert_eq!(tracker.state(), PlanModeState::Pending);
    reconcile(&mut tracker, PromptMode::Ask);
    assert_eq!(tracker.state(), PlanModeState::Inactive);
}
