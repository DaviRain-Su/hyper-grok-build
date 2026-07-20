//! Codex subscription connector backed by `codex app-server`.
//!
//! This is the thin `grok codex` CLI: it spawns a `codex app-server`,
//! authenticates against the user's ChatGPT/OpenAI subscription, and drives a
//! single thread interactively. The pager's in-process routing through ACP
//! lives in `crate::acp::router` and shares the same `CodexAppServer` client
//! defined in `crate::codex_app_server`.

use crate::app::cli::CodexArgs;
use crate::codex_app_server::{CodexAppServer, danger_full_access_sandbox, workspace_write_sandbox};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Write as _;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Sandbox string sent to `thread/start` / `thread/resume`.
fn thread_sandbox(full_access: bool) -> &'static str {
    if full_access {
        "danger-full-access"
    } else {
        "workspace-write"
    }
}

/// Sandbox policy sent to `turn/start`.
fn turn_sandbox(full_access: bool, cwd: &std::path::Path) -> Value {
    if full_access {
        danger_full_access_sandbox()
    } else {
        workspace_write_sandbox(cwd)
    }
}

/// Run the `grok codex` connector.
pub async fn run(args: CodexArgs) -> Result<()> {
    // `CodexAppServer` is single-threaded: its reader/writer tasks are
    // `spawn_local` jobs sharing an `Rc` pending-request map. The pager's
    // agent thread already runs inside a `LocalSet`; the CLI entry point
    // runs on the main multi-thread runtime, so drive the connector inside
    // a `LocalSet` here to give those spawns a home.
    let local = tokio::task::LocalSet::new();
    local.run_until(run_inner(args)).await
}

async fn run_inner(args: CodexArgs) -> Result<()> {
    let server = CodexAppServer::start(&args.codex_binary).await?;
    let status = server.status().await?;
    if args.status {
        println!("Codex authentication: {}", status.account_label);
        println!(
            "Default model: {}",
            status
                .default_model
                .as_deref()
                .unwrap_or("Codex configuration default")
        );
        println!("Available models:");
        for model in &status.models {
            println!("  {}", model.id);
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let mut model = args.model.or(status.default_model);
    let thread_id = server
        .open_thread(
            &cwd,
            model.as_deref(),
            args.resume.as_deref(),
            thread_sandbox(args.full_access),
        )
        .await
        .with_context(|| "could not start Codex thread")?;

    let initial_prompt = args.prompt.or(args.message);
    if let Some(prompt) = initial_prompt {
        run_turn(
            &server,
            &thread_id,
            &prompt,
            model.as_deref(),
            args.full_access,
            &cwd,
        )
        .await?;
        eprintln!("\x1b[2mCodex thread: {thread_id}\x1b[0m");
        return Ok(());
    }

    eprintln!(
        "Codex • {} • model {}",
        status.account_label,
        model.as_deref().unwrap_or("default")
    );
    eprintln!("Thread {thread_id} • /help for commands");
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        eprint!("\n\x1b[1;36m›\x1b[0m ");
        std::io::stderr().flush()?;
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        match prompt {
            "/exit" | "/quit" => break,
            "/help" => {
                eprintln!("/model <id>  switch model for following turns");
                eprintln!("/models      list subscription models");
                eprintln!("/thread      print the resumable Codex thread id");
                eprintln!("/exit        quit");
            }
            "/models" => {
                for available in &status.models {
                    eprintln!("{}", available.id);
                }
            }
            "/thread" => eprintln!("{thread_id}"),
            _ if prompt.starts_with("/model ") => {
                let requested = prompt.trim_start_matches("/model ").trim();
                if requested.is_empty() {
                    eprintln!("usage: /model <id>");
                } else {
                    model = Some(requested.to_owned());
                    eprintln!("model: {requested}");
                }
            }
            _ => {
                if let Err(error) = run_turn(
                    &server,
                    &thread_id,
                    prompt,
                    model.as_deref(),
                    args.full_access,
                    &cwd,
                )
                .await
                {
                    eprintln!("Codex error: {error:#}");
                }
            }
        }
    }
    Ok(())
}

/// Run one turn against `thread_id`, streaming deltas to stdout and tool
/// activity to stderr. Returns when the turn completes or the server reports
/// an error.
async fn run_turn(
    server: &CodexAppServer,
    thread_id: &str,
    prompt: &str,
    model: Option<&str>,
    full_access: bool,
    cwd: &std::path::Path,
) -> Result<()> {
    // Subscribe before starting the turn so we never miss the first
    // notification (broadcast fans out from the moment of subscribe).
    let mut notifications = server.subscribe();
    let turn_id = server
        .start_turn_with(
            thread_id,
            vec![json!({ "type": "text", "text": prompt })],
            model,
            None,
            turn_sandbox(full_access, cwd),
        )
        .await?;
    let mut wrote_text = false;

    loop {
        let notification = match notifications.recv().await {
            Ok(notification) => notification,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!("\x1b[2m[codex: lagged {skipped} notifications]\x1b[0m");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                bail!("Codex app-server notification stream closed");
            }
        };
        if notification
            .pointer("/params/threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != thread_id)
        {
            continue;
        }
        match notification.get("method").and_then(Value::as_str) {
            Some("item/agentMessage/delta" | "item/plan/delta") => {
                if let Some(delta) = notification
                    .pointer("/params/delta")
                    .and_then(Value::as_str)
                {
                    print!("{delta}");
                    std::io::stdout().flush()?;
                    wrote_text = true;
                }
            }
            Some("item/started") => {
                if let Some(label) = tool_label(notification.pointer("/params/item")) {
                    eprintln!("\x1b[2m[{label}]\x1b[0m");
                }
            }
            Some("error") if notification.pointer("/params/willRetry") != Some(&Value::Bool(true)) => {
                let message = notification
                    .pointer("/params/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex app-server reported an error");
                bail!("{message}");
            }
            Some("turn/completed") => {
                if notification
                    .pointer("/params/turn/id")
                    .and_then(Value::as_str)
                    != Some(turn_id.as_str())
                {
                    continue;
                }
                let status = notification
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                if status == "failed" {
                    let message = notification
                        .pointer("/params/turn/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex turn failed");
                    bail!("{message}");
                }
                break;
            }
            _ => {}
        }
    }
    if wrote_text {
        println!();
    }
    Ok(())
}

fn tool_label(item: Option<&Value>) -> Option<String> {
    let item = item?;
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => Some(format!(
            "shell: {}",
            item.get("command")
                .and_then(Value::as_str)
                .unwrap_or("command")
        )),
        "fileChange" => Some("apply patch".to_owned()),
        "mcpToolCall" => Some(format!(
            "{}.{}",
            item.get("server").and_then(Value::as_str).unwrap_or("mcp"),
            item.get("tool").and_then(Value::as_str).unwrap_or("tool")
        )),
        "webSearch" => Some("web search".to_owned()),
        "collabAgentToolCall" => Some("collaboration agent".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_command_and_mcp_activity() {
        assert_eq!(
            tool_label(Some(
                &json!({ "type": "commandExecution", "command": "cargo test" })
            )),
            Some("shell: cargo test".to_owned())
        );
        assert_eq!(
            tool_label(Some(
                &json!({ "type": "mcpToolCall", "server": "docs", "tool": "search" })
            )),
            Some("docs.search".to_owned())
        );
    }

    #[test]
    fn sandbox_selectors_match_full_access_flag() {
        assert_eq!(thread_sandbox(false), "workspace-write");
        assert_eq!(thread_sandbox(true), "danger-full-access");
        assert_eq!(
            turn_sandbox(false, std::path::Path::new("/tmp/proj"))["type"],
            "workspaceWrite"
        );
        assert_eq!(turn_sandbox(true, std::path::Path::new("/tmp/proj"))["type"], "dangerFullAccess");
    }
}