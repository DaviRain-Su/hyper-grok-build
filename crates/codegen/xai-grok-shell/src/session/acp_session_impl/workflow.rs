use std::sync::Arc;

use super::super::acp_session::SessionActor;

impl SessionActor {
    pub(crate) fn named_workflow_snapshot(
        &self,
    ) -> (
        crate::session::workflow::registry::WorkflowRegistry,
        Vec<crate::session::workflow::registry::WorkflowListing>,
    ) {
        crate::session::workflow::registry::workflow_snapshot(Some(std::path::Path::new(
            self.session_info.cwd.as_str(),
        )))
    }

    pub(crate) async fn launch_named_workflow(
        self: &Arc<Self>,
        registry: &crate::session::workflow::registry::WorkflowRegistry,
        name: &str,
        input: &str,
    ) -> String {
        let resolved = match registry.resolve_by_name(name) {
            Ok(r) => r,
            Err(e) => return format!("Workflow '{name}' unavailable: {e}"),
        };
        let (args, objective) = parse_named_workflow_args(input, &resolved.meta.description);
        let spec = crate::session::workflow::manager::LaunchSpec {
            objective,
            args,
            agent_budget: None,
            resume_run_id: None,
        };
        let launched = self.workflow_manager.lock().await.launch(resolved, spec);
        match launched {
            Ok((run_id, outcome_rx)) => {
                let (display, objective) = self
                    .workflow_tracker()
                    .await
                    .lock()
                    .get(&run_id)
                    .map(|r| (r.name.clone(), r.objective.clone()))
                    .unwrap_or_else(|| (name.to_string(), String::new()));
                let command_line = if input.trim().is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {}", input.trim())
                };
                self.push_workflow_launch_reminder(
                    &display,
                    &run_id,
                    &objective,
                    &command_line,
                    false,
                );
                tokio::spawn(async move {
                    if let Ok(outcome) = outcome_rx.await {
                        tracing::info!(run_id, ?outcome, "named workflow finished");
                    }
                });
                format!(
                    "Workflow '{display}' started in the background. Watch it in /workflows; \
                     the result lands here when it finishes."
                )
            }
            Err(e) => format!("Could not start workflow '{name}': {e}"),
        }
    }

    pub(crate) async fn manage_workflow_run(self: &Arc<Self>, run_id: &str, op: &str) -> String {
        use crate::session::workflow::tracker::WorkflowRunStatus;

        const USAGE: &str = "Usage: /workflow <name> [args] to launch a saved workflow, or \
                             /workflow <op> [name] (also `/workflow <name> <op>`) to manage \
                             a run — ops: pause, resume, stop, save.";
        if op.is_empty() {
            return USAGE.to_string();
        }

        let matches: Vec<(String, WorkflowRunStatus, String)> = {
            let tracker = self.workflow_tracker().await;
            let tracker = tracker.lock();
            let all: Vec<_> = tracker
                .list()
                .iter()
                .filter(|r| r.run_id.starts_with(run_id) || r.name.starts_with(run_id))
                .map(|r| (r.run_id.clone(), r.status, r.name.clone()))
                .collect();
            narrow_run_matches(all, run_id, op)
        };
        let (full_id, status, name) = match matches.as_slice() {
            [] if run_id.is_empty() => {
                return "No workflow runs in this session yet.".to_string();
            }
            [] => return format!("No workflow run matches '{run_id}'."),
            [one] => one.clone(),
            many => {
                let rows: Vec<String> = many
                    .iter()
                    .map(|(_, status, name)| format!("  {name} ({})", status.as_str()))
                    .collect();
                return format!(
                    "Several runs could be '{op}' — pick one by name:\n{}\n(/workflow {op} <name>)",
                    rows.join("\n")
                );
            }
        };
        let id_suffix = format!(" {name}");

        match op {
            "pause" => {
                if status != WorkflowRunStatus::Active {
                    return format!("Run '{name}' is not active (status: {}).", status.as_str());
                }
                self.workflow_manager.lock().await.pause(&full_id);
                format!("Paused {name}. /workflow resume{id_suffix} to continue.")
            }
            "stop" => {
                if status.is_terminal() {
                    return format!(
                        "Run '{name}' is already finished (status: {}).",
                        status.as_str()
                    );
                }
                self.workflow_manager.lock().await.cancel(&full_id);
                format!("Stopped {name}.")
            }
            "resume" => {
                if status == WorkflowRunStatus::Active {
                    return format!("Run '{name}' is already running.");
                }
                if !status.is_paused() {
                    return format!(
                        "Run '{name}' cannot be resumed (status: {}). Start a new run instead.",
                        status.as_str()
                    );
                }
                if status == WorkflowRunStatus::BudgetLimited {
                    let (used, limit) = {
                        let tracker = self.workflow_tracker().await;
                        let tracker = tracker.lock();
                        let run = tracker.get(&full_id);
                        (
                            run.as_ref().map_or(0, |r| r.agents_used),
                            run.as_ref().and_then(|r| r.agent_budget),
                        )
                    };
                    let limit = limit.map_or_else(String::new, |l| format!("/{l}"));
                    if used >= xai_workflow::MAX_AGENT_BUDGET {
                        return format!(
                            "Run '{name}' exhausted the maximum agent budget ({used}{limit} agents) \
                             and cannot be resumed. Start a new run instead."
                        );
                    }
                    let suggested = used.saturating_add(64).min(xai_workflow::MAX_AGENT_BUDGET);
                    return format!(
                        "Run '{name}' exhausted its agent budget ({used}{limit} agents). \
                         Resuming keeps all finished work but needs a higher absolute cap — \
                         ask the agent to resume it with an agent budget above {used}, e.g. \
                         \"resume {name} with an agent budget of {suggested}\"."
                    );
                }
                let (script, args) = {
                    let manager = self.workflow_manager.lock().await;
                    (
                        manager.script_copy_for(&full_id),
                        manager.args_copy_for(&full_id),
                    )
                };
                let Some(script) = script else {
                    return format!("No persisted script for '{name}'; cannot resume.");
                };
                let resolved = match crate::session::workflow::registry::resolve_inline(script) {
                    Ok(r) => r,
                    Err(e) => return format!("Persisted script invalid: {e}"),
                };
                let objective = {
                    let tracker = self.workflow_tracker().await;
                    tracker
                        .lock()
                        .get(&full_id)
                        .map(|r| r.objective.clone())
                        .unwrap_or_default()
                };
                let agent_budget = {
                    let tracker = self.workflow_tracker().await;
                    tracker
                        .lock()
                        .get(&full_id)
                        .and_then(|run| run.agent_budget)
                };
                let objective_echo = objective.clone();
                let spec = crate::session::workflow::manager::LaunchSpec {
                    objective,
                    args,
                    agent_budget,
                    resume_run_id: Some(full_id.clone()),
                };
                match self.workflow_manager.lock().await.launch(resolved, spec) {
                    Ok((rid, outcome_rx)) => {
                        tokio::spawn(async move {
                            if let Ok(outcome) = outcome_rx.await {
                                tracing::info!(run_id = rid, ?outcome, "resumed workflow finished");
                            }
                        });
                        self.push_workflow_launch_reminder(
                            &name,
                            &full_id,
                            &objective_echo,
                            &format!("/workflow resume {name}"),
                            true,
                        );
                        format!("Resumed {name} from its journal.")
                    }
                    Err(e) => format!("Could not resume '{name}': {e}"),
                }
            }
            "save" => {
                let Some(script) = self.workflow_manager.lock().await.script_copy_for(&full_id)
                else {
                    return format!("No persisted script for '{name}'; nothing to save.");
                };
                let definition_name =
                    match crate::session::workflow::registry::resolve_inline(script.clone()) {
                        Ok(resolved) => resolved.meta.name,
                        Err(error) => return format!("Could not save workflow '{name}': {error}"),
                    };
                if definition_name != name {
                    return format!(
                        "Save is disabled for run '{name}': it is a duplicate-run display handle, \
                         while the script is still named '{definition_name}'. Choose a new unique \
                         meta.name and save the script under that name instead."
                    );
                }
                if crate::session::workflow::registry::BUILTIN_WORKFLOWS
                    .iter()
                    .any(|builtin| builtin.name == definition_name)
                {
                    return format!(
                        "Save is disabled for built-in workflow '{definition_name}', which is \
                         already runnable. To customize it, create a copy with a new unique \
                         meta.name."
                    );
                }
                match crate::session::workflow::registry::save_project_workflow(
                    std::path::Path::new(self.session_info.cwd.as_str()),
                    &definition_name,
                    &script,
                ) {
                    Ok(path) => format!(
                        "Saved workflow '{definition_name}' to {} — runnable by name from now on.",
                        path.display()
                    ),
                    Err(e) => format!("Could not save workflow '{definition_name}': {e}"),
                }
            }
            other => format!("Unknown op '{other}'. {USAGE}"),
        }
    }
}

pub(crate) fn parse_named_workflow_args(
    input: &str,
    description: &str,
) -> (serde_json::Value, String) {
    let input = input.trim();
    if input.is_empty() {
        return (serde_json::Value::Null, description.to_string());
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(input) {
        let objective = map
            .get("objective")
            .or_else(|| map.get("query"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string());
        return (serde_json::Value::Object(map), objective);
    }
    (
        serde_json::json!({ "query": input, "objective": input }),
        input.to_string(),
    )
}

type RunMatch = (
    String,
    crate::session::workflow::tracker::WorkflowRunStatus,
    String,
);

fn narrow_run_matches(mut all: Vec<RunMatch>, selector: &str, op: &str) -> Vec<RunMatch> {
    use crate::session::workflow::tracker::WorkflowRunStatus;
    if !selector.is_empty() {
        let exact: Vec<_> = all
            .iter()
            .filter(|(id, _, name)| id.as_str() == selector || name.as_str() == selector)
            .cloned()
            .collect();
        if !exact.is_empty() {
            all = exact;
        }
    }
    if all.len() > 1 {
        let applicable: Vec<_> = all
            .iter()
            .filter(|(_, status, ..)| match op {
                "pause" => *status == WorkflowRunStatus::Active,
                "resume" => status.is_paused(),
                "stop" => !status.is_terminal(),
                _ => true,
            })
            .cloned()
            .collect();
        if applicable.len() == 1 {
            return applicable;
        }
    }
    all
}

#[cfg(test)]
mod run_match_tests {
    use super::narrow_run_matches;
    use crate::session::workflow::tracker::WorkflowRunStatus;

    fn run(id: &str, name: &str, status: WorkflowRunStatus) -> super::RunMatch {
        (id.to_string(), status, name.to_string())
    }

    #[test]
    fn exact_name_beats_prefix_of_uniquified_sibling() {
        let all = vec![
            run("wf_1", "deep-research", WorkflowRunStatus::Active),
            run("wf_2", "deep-research-2", WorkflowRunStatus::Active),
        ];
        let picked = narrow_run_matches(all, "deep-research", "stop");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "deep-research");
    }

    #[test]
    fn prefix_still_narrows_by_op_applicability() {
        let all = vec![
            run("wf_1", "deep-research", WorkflowRunStatus::Complete),
            run("wf_2", "deep-research-2", WorkflowRunStatus::Active),
        ];
        let picked = narrow_run_matches(all, "deep", "stop");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "deep-research-2");
    }

    #[test]
    fn empty_selector_with_single_applicable_run_resolves() {
        let all = vec![
            run("wf_1", "a", WorkflowRunStatus::Complete),
            run("wf_2", "b", WorkflowRunStatus::UserPaused),
        ];
        let picked = narrow_run_matches(all, "", "resume");
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "b");
    }

    #[test]
    fn ambiguous_stays_ambiguous() {
        let all = vec![
            run("wf_1", "a", WorkflowRunStatus::Active),
            run("wf_2", "b", WorkflowRunStatus::Active),
        ];
        assert_eq!(narrow_run_matches(all, "", "stop").len(), 2);
    }
}
