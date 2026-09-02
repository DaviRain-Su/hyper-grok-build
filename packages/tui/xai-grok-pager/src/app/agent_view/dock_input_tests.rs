//! Regression tests for dock-focused input: modifiers, hidden-focus, and empty-dock toggles.

use super::test_fixtures::make_agent;
use super::{AgentPane, AgentView};
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

fn insert_running_task(agent: &mut AgentView, task_id: &str) {
    agent.session.bg_tasks.insert(
        task_id.into(),
        crate::app::agent::BgTaskState {
            task_id: task_id.into(),
            tool_call_id: format!("call-{task_id}"),
            command: "sleep 5".into(),
            description: None,
            cwd: "/tmp".into(),
            output_file: "/tmp/out".into(),
            status: crate::app::agent::BgTaskStatus::Running,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor: false,
            restored_from_replay: false,
        },
    );
}

fn dock_with_task() -> AgentView {
    let mut agent = make_agent();
    insert_running_task(&mut agent, "bg-1");
    agent.dock_shown = true;
    agent.dock_on = true;
    agent.active_pane = AgentPane::Dock;
    agent
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn ctrl_q_while_dock_focused_is_unchanged() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "Ctrl+Q must bubble to global Quit, got {outcome:?}"
    );
    assert_eq!(agent.active_pane, AgentPane::Dock);
}

#[test]
fn ctrl_x_while_dock_focused_is_unchanged() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "Ctrl+X must not take the kill arm, got {outcome:?}"
    );
}

#[test]
fn unmodified_q_unfocuses_dock() {
    let mut agent = dock_with_task();
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Changed));
    assert_eq!(agent.active_pane, AgentPane::Scrollback);
}

#[test]
fn unmodified_x_kills_selected_task() {
    let mut agent = dock_with_task();
    agent.dock_cursor = agent
        .dock_items()
        .iter()
        .position(|item| {
            matches!(
                item,
                crate::views::dock::DockItem::Row(crate::views::dock::Section::Tasks, 0)
            )
        })
        .expect("expanded Tasks section has a row");
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(matches!(
        outcome,
        InputOutcome::Action(Action::KillBgTask(id)) if id == "bg-1"
    ));
}

#[test]
fn hidden_dock_does_not_navigate_or_kill() {
    let mut agent = dock_with_task();
    agent.dock_shown = false;
    let outcome = agent.handle_dock_key(&key(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert_eq!(agent.dock_cursor, 0);

    let outcome = agent.handle_dock_key(&key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(
        !matches!(outcome, InputOutcome::Action(_)),
        "x on a hidden dock must not kill, got {outcome:?}"
    );
}

#[test]
fn toggle_tasks_does_not_open_hidden_pane_when_dock_on_but_empty() {
    let mut agent = make_agent();
    agent.dock_on = true;
    agent.dock_shown = false;
    agent.tasks.overlay.visible = false;
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(matches!(outcome, InputOutcome::Unchanged));
    assert!(!agent.tasks.overlay.visible);
    assert_ne!(agent.active_pane, AgentPane::Tasks);
}

#[test]
fn toggle_tasks_still_toggles_legacy_pane_when_dock_off() {
    let mut agent = make_agent();
    agent.dock_on = false;
    agent.dock_shown = false;
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(matches!(outcome, InputOutcome::Changed));
    assert!(agent.tasks.overlay.visible);
    assert_eq!(agent.active_pane, AgentPane::Tasks);
}

#[test]
fn toggle_queue_does_not_open_hidden_pane_when_dock_on_but_empty() {
    let mut agent = make_agent();
    agent.dock_on = true;
    agent.dock_shown = false;
    agent.queue.overlay.visible = false;
    agent
        .session
        .pending_prompts
        .push_back(crate::app::agent::QueuedPrompt::plain(
            1,
            "queued",
            crate::app::agent::QueueEntryKind::Prompt,
        ));
    let registry = ActionRegistry::defaults();
    let outcome = agent.handle_input(
        &Event::Key(key(KeyCode::Char(';'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(
        matches!(outcome, InputOutcome::Unchanged),
        "empty dock must not toggle the suppressed queue pane, got {outcome:?}"
    );
    assert!(!agent.queue.overlay.visible);
}
