use anyhow::Result;
use tokio_util::sync::CancellationToken;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::session::share::{ShareSessionRequest, ShareSessionResponse};

use agent_client_protocol as acp;
use xai_acp_lib::acp_send;

#[derive(Debug, clap::Args, Clone)]
pub struct ShareArgs {
    /// Session ID to share
    pub session_id: String,
}

pub async fn run(args: &ShareArgs, agent_config: &AgentConfig) -> Result<()> {
    let cancel = CancellationToken::new();
    let spawned = crate::acp::spawn::spawn_grok_shell(agent_config.clone(), &cancel, None).await?;
    // Cancel + join on every return path, including the `?`s below.
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

    let params = serde_json::value::to_raw_value(&ShareSessionRequest {
        session_id: args.session_id.clone(),
    })?;
    let ext_req = acp::ExtRequest::new("x.ai/share_session", params.into());

    let ext_resp: acp::ExtResponse = acp_send(ext_req, &spawned.channel.tx).await?;
    let response: ShareSessionResponse = serde_json::from_str(ext_resp.0.get())?;

    println!("{}", response.share_url);
    Ok(())
}
