mod display;

use anyhow::{Result, bail};
use clap::Subcommand;
use tokio_util::sync::CancellationToken;
use xai_fast_worktree::WorktreeRecord;

use agent_client_protocol as acp;
use xai_acp_lib::acp_send;
use xai_grok_shell::agent::config::Config as AgentConfig;

/// Local response types matching the ACP response shapes.
#[derive(Debug, serde::Deserialize)]
pub struct GcReport {
    pub dead_removed: u64,
    pub expired_removed: u64,
    pub skipped_alive: u64,
    // serde(default) so reports from agents predating this field still parse.
    #[serde(default)]
    pub remove_failed: u64,
}

#[derive(Debug, serde::Deserialize)]
pub struct DbStats {
    pub total_records: u64,
    pub alive_count: u64,
    pub dead_count: u64,
    pub db_file_bytes: u64,
}

#[derive(Debug, serde::Deserialize)]
pub struct RebuildReport {
    pub discovered: u64,
    pub registered: u64,
    pub already_tracked: u64,
}

#[derive(Debug, clap::Args, Clone)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    command: WorktreeCommand,
}

#[derive(Debug, Subcommand, Clone)]
enum WorktreeCommand {
    /// List tracked worktrees
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, value_delimiter = ',')]
        r#type: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        all: bool,
    },
    /// Show details for a specific worktree
    Show { id_or_path: String },
    /// Remove worktrees
    Rm {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Garbage-collect orphaned/stale worktrees
    #[command(alias = "prune")]
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        max_age: Option<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// Database maintenance
    Db {
        #[command(subcommand)]
        command: WorktreeDbCommand,
    },
}

#[derive(Debug, Subcommand, Clone)]
enum WorktreeDbCommand {
    /// Rebuild DB from filesystem scan
    Rebuild,
    /// Show DB statistics
    Stats,
    /// Print DB file path
    Path,
}

pub async fn run(args: WorktreeArgs, agent_config: &AgentConfig) -> Result<()> {
    let cancel = CancellationToken::new();
    let spawned = crate::acp::spawn::spawn_grok_shell(agent_config.clone(), &cancel, None).await?;
    // Cancel + join on every return path, including the `?` below.
    let _agent_guard =
        crate::acp::spawn::AgentShutdownGuard::new(cancel.clone(), Some(spawned.thread_handle));

    let _init: acp::InitializeResponse = acp_send(
        acp::InitializeRequest::new(acp::ProtocolVersion::V1)
            .client_capabilities(
                acp::ClientCapabilities::new()
                    .fs(acp::FileSystemCapabilities::new())
                    .terminal(false),
            )
            .meta(
                serde_json::json!({
                    "clientType": crate::client_identity::HEADLESS_CLIENT_TYPE,
                    "clientVersion": crate::client_identity::PAGER_CLIENT_VERSION
                })
                .as_object()
                .cloned(),
            ),
        &spawned.channel.tx,
    )
    .await?;

    dispatch(args.command, &spawned.channel.tx).await
}

async fn dispatch(command: WorktreeCommand, tx: &xai_acp_lib::AcpAgentTx) -> Result<()> {
    match command {
        WorktreeCommand::List {
            repo,
            r#type,
            json,
            all,
        } => cmd_list(tx, repo, r#type, json, all).await,
        WorktreeCommand::Show { id_or_path } => cmd_show(tx, &id_or_path).await,
        WorktreeCommand::Rm {
            ids,
            force,
            dry_run,
        } => cmd_rm(tx, ids, force, dry_run).await,
        WorktreeCommand::Gc {
            dry_run,
            max_age,
            force,
        } => cmd_gc(tx, dry_run, max_age, force).await,
        WorktreeCommand::Db { command } => cmd_db(tx, command).await,
    }
}

fn ext_request<T: serde::Serialize>(
    method: &str,
    params: &T,
) -> Result<acp::ExtRequest, serde_json::Error> {
    let params = serde_json::value::to_raw_value(params)?;
    Ok(acp::ExtRequest::new(method, params.into()))
}

/// ACP extension responses are wrapped in `{ "result": T, "error": ... }`.
#[derive(serde::Deserialize)]
struct ExtEnvelope<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

async fn ext_call<T: serde::de::DeserializeOwned>(
    tx: &xai_acp_lib::AcpAgentTx,
    method: &str,
    params: &impl serde::Serialize,
) -> Result<T> {
    let req =
        ext_request(method, params).map_err(|e| anyhow::anyhow!("failed to build request: {e}"))?;
    let resp = acp_send(req, tx)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let envelope: ExtEnvelope<T> = serde_json::from_str(resp.0.get())
        .map_err(|e| anyhow::anyhow!("response parse error: {e}"))?;
    if let Some(err) = envelope.error {
        bail!("ACP error: {err}");
    }
    envelope
        .result
        .ok_or_else(|| anyhow::anyhow!("ACP response missing result field"))
}

async fn cmd_list(
    tx: &xai_acp_lib::AcpAgentTx,
    repo: Option<String>,
    types: Vec<String>,
    json: bool,
    all: bool,
) -> Result<()> {
    let records: Vec<WorktreeRecord> = ext_call(
        tx,
        "x.ai/git/worktree/list",
        &serde_json::json!({
            "repo": repo,
            "type": types,
            "includeAll": all,
        }),
    )
    .await?;

    if json {
        display::print_json(&records);
    } else {
        display::print_table(&records);
    }
    Ok(())
}

async fn cmd_show(tx: &xai_acp_lib::AcpAgentTx, id_or_path: &str) -> Result<()> {
    let rec: Option<WorktreeRecord> = ext_call(
        tx,
        "x.ai/git/worktree/show",
        &serde_json::json!({ "idOrPath": id_or_path }),
    )
    .await?;

    match rec {
        Some(r) => {
            display::print_show(&r);
            Ok(())
        }
        None => bail!("worktree not found: {id_or_path}"),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveResponse {
    removed: bool,
    #[serde(default)]
    resolved_path: Option<String>,
}

async fn cmd_rm(
    tx: &xai_acp_lib::AcpAgentTx,
    ids: Vec<String>,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    for id_or_path in &ids {
        let resp: Result<RemoveResponse> = ext_call(
            tx,
            "x.ai/git/worktree/remove",
            &serde_json::json!({
                "idOrPath": id_or_path,
                "force": force,
                "dryRun": dry_run,
            }),
        )
        .await;

        match resp {
            Ok(r) => {
                let path = r.resolved_path.as_deref().unwrap_or(id_or_path);
                if dry_run {
                    println!("  would remove: {path}");
                } else if r.removed {
                    println!("  removed: {path}");
                }
            }
            Err(e) => eprintln!("  error removing {id_or_path}: {e}"),
        }
    }
    Ok(())
}

async fn cmd_gc(
    tx: &xai_acp_lib::AcpAgentTx,
    dry_run: bool,
    max_age: Option<String>,
    force: bool,
) -> Result<()> {
    let report: GcReport = ext_call(
        tx,
        "x.ai/git/worktree/gc",
        &serde_json::json!({
            "dryRun": dry_run,
            "maxAge": max_age,
            "force": force,
        }),
    )
    .await?;

    if dry_run {
        println!("Dry run \u{2014} no changes made.");
    }
    display::print_gc(&report);
    Ok(())
}

async fn cmd_db(tx: &xai_acp_lib::AcpAgentTx, command: WorktreeDbCommand) -> Result<()> {
    match command {
        WorktreeDbCommand::Stats => {
            let stats: DbStats = ext_call(tx, "x.ai/git/worktree/db/stats", &()).await?;
            display::print_stats(&stats);
            Ok(())
        }
        WorktreeDbCommand::Path => {
            #[derive(serde::Deserialize)]
            struct PathResp {
                path: String,
            }
            let resp: PathResp = ext_call(tx, "x.ai/git/worktree/db/path", &()).await?;
            println!("{}", resp.path);
            Ok(())
        }
        WorktreeDbCommand::Rebuild => {
            let report: RebuildReport = ext_call(tx, "x.ai/git/worktree/db/rebuild", &()).await?;
            display::print_rebuild(&report);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_request_builds_list_with_filters() {
        let req = ext_request(
            "x.ai/git/worktree/list",
            &serde_json::json!({
                "repo": "xai",
                "type": ["session"],
                "includeAll": true,
            }),
        )
        .unwrap();
        assert_eq!(req.method.as_ref(), "x.ai/git/worktree/list");
        let params: serde_json::Value = serde_json::from_str(req.params.get()).unwrap();
        assert_eq!(params["repo"], "xai");
        assert_eq!(params["includeAll"], true);
    }

    #[test]
    fn ext_request_builds_gc_with_max_age_string() {
        let req = ext_request(
            "x.ai/git/worktree/gc",
            &serde_json::json!({
                "dryRun": true,
                "maxAge": "7d",
                "force": false,
            }),
        )
        .unwrap();
        let params: serde_json::Value = serde_json::from_str(req.params.get()).unwrap();
        assert_eq!(params["maxAge"], "7d");
        assert_eq!(params["dryRun"], true);
    }

    #[test]
    fn ext_request_builds_remove_with_id_or_path() {
        let req = ext_request(
            "x.ai/git/worktree/remove",
            &serde_json::json!({
                "idOrPath": "wt-abc123",
                "force": true,
                "dryRun": false,
            }),
        )
        .unwrap();
        let params: serde_json::Value = serde_json::from_str(req.params.get()).unwrap();
        assert_eq!(params["idOrPath"], "wt-abc123");
    }

    #[test]
    fn ext_request_builds_show() {
        let req = ext_request(
            "x.ai/git/worktree/show",
            &serde_json::json!({ "idOrPath": "/some/path" }),
        )
        .unwrap();
        let params: serde_json::Value = serde_json::from_str(req.params.get()).unwrap();
        assert_eq!(params["idOrPath"], "/some/path");
    }

    #[test]
    fn ext_request_builds_db_stats_empty_params() {
        let req = ext_request("x.ai/git/worktree/db/stats", &()).unwrap();
        assert_eq!(req.method.as_ref(), "x.ai/git/worktree/db/stats");
    }

    #[test]
    fn remove_response_deserializes_with_resolved_path() {
        let json = r#"{"removed": true, "resolvedPath": "/resolved"}"#;
        let resp: RemoveResponse = serde_json::from_str(json).unwrap();
        assert!(resp.removed);
        assert_eq!(resp.resolved_path.as_deref(), Some("/resolved"));
    }

    #[test]
    fn remove_response_deserializes_without_resolved_path() {
        let json = r#"{"removed": true}"#;
        let resp: RemoveResponse = serde_json::from_str(json).unwrap();
        assert!(resp.removed);
        assert!(resp.resolved_path.is_none());
    }

    #[test]
    fn ext_envelope_unwraps_success_result() {
        let json = r#"{"result": {"path": "/home/user/.grok/worktrees.db"}, "error": null}"#;
        #[derive(serde::Deserialize)]
        struct PathResp {
            path: String,
        }
        let envelope: ExtEnvelope<PathResp> = serde_json::from_str(json).unwrap();
        assert!(envelope.error.is_none());
        let inner = envelope.result.unwrap();
        assert_eq!(inner.path, "/home/user/.grok/worktrees.db");
    }

    #[test]
    fn ext_envelope_unwraps_error_result() {
        let json = r#"{"result": null, "error": "something went wrong"}"#;
        let envelope: ExtEnvelope<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert!(envelope.result.is_none());
        assert!(envelope.error.is_some());
    }

    #[test]
    fn ext_envelope_unwraps_list_of_records() {
        let json = r#"{"result": [], "error": null}"#;
        let envelope: ExtEnvelope<Vec<WorktreeRecord>> = serde_json::from_str(json).unwrap();
        assert!(envelope.error.is_none());
        assert!(envelope.result.unwrap().is_empty());
    }

    #[test]
    fn ext_envelope_unwraps_db_stats() {
        let json = r#"{"result": {"total_records": 5, "alive_count": 3, "dead_count": 2, "db_file_bytes": 1024}}"#;
        let envelope: ExtEnvelope<DbStats> = serde_json::from_str(json).unwrap();
        let stats = envelope.result.unwrap();
        assert_eq!(stats.total_records, 5);
        assert_eq!(stats.alive_count, 3);
    }

    #[test]
    fn ext_envelope_unwraps_gc_report() {
        let json = r#"{"result": {"dead_removed": 2, "expired_removed": 1, "skipped_alive": 0}}"#;
        let envelope: ExtEnvelope<GcReport> = serde_json::from_str(json).unwrap();
        let report = envelope.result.unwrap();
        assert_eq!(report.dead_removed, 2);
        assert_eq!(report.expired_removed, 1);
        // Older agents omit remove_failed; it must default to zero.
        assert_eq!(report.remove_failed, 0);
    }

    #[test]
    fn rm_parses_short_force_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: WorktreeCommand,
        }

        let cli = Cli::parse_from(["test", "rm", "-f", "wt-1"]);
        match cli.command {
            WorktreeCommand::Rm {
                ids,
                force,
                dry_run,
            } => {
                assert!(force);
                assert!(!dry_run);
                assert_eq!(ids, vec!["wt-1"]);
            }
            _ => panic!("expected Rm variant"),
        }
    }

    #[test]
    fn rm_parses_long_force_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: WorktreeCommand,
        }

        let cli = Cli::parse_from(["test", "rm", "--force", "a", "b"]);
        match cli.command {
            WorktreeCommand::Rm {
                ids,
                force,
                dry_run,
            } => {
                assert!(force);
                assert!(!dry_run);
                assert_eq!(ids, vec!["a", "b"]);
            }
            _ => panic!("expected Rm variant"),
        }
    }

    #[test]
    fn gc_parses_short_force_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: WorktreeCommand,
        }

        let cli = Cli::parse_from(["test", "gc", "-f"]);
        match cli.command {
            WorktreeCommand::Gc {
                force,
                dry_run,
                max_age,
            } => {
                assert!(force);
                assert!(!dry_run);
                assert!(max_age.is_none());
            }
            _ => panic!("expected Gc variant"),
        }
    }

    #[test]
    fn list_accepts_ls_alias() {
        use clap::Parser;

        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            command: WorktreeCommand,
        }

        let cli = Cli::parse_from(["test", "ls", "--json"]);
        match cli.command {
            WorktreeCommand::List {
                repo,
                r#type,
                json,
                all,
            } => {
                assert!(repo.is_none());
                assert!(r#type.is_empty());
                assert!(json);
                assert!(!all);
            }
            _ => panic!("expected List variant"),
        }
    }
}
