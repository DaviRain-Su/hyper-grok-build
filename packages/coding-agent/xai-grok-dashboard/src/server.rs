//! Axum server and Leptos SSR integration.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, bail};
use axum::extract::FromRef;
use axum::{Router, routing::get};
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, generate_route_list};

use crate::api;
use crate::app::App;
use crate::store::DashboardStore;

#[derive(Clone)]
pub struct AppState {
    leptos_options: LeptosOptions,
    store: Arc<DashboardStore>,
}

impl FromRef<AppState> for LeptosOptions {
    fn from_ref(state: &AppState) -> Self {
        state.leptos_options.clone()
    }
}

impl FromRef<AppState> for Arc<DashboardStore> {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

#[derive(Debug, Clone)]
pub struct DashboardServerConfig {
    pub bind: SocketAddr,
    pub open_browser: bool,
    pub grok_home: std::path::PathBuf,
}

impl DashboardServerConfig {
    pub fn new(grok_home: std::path::PathBuf) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9090)),
            open_browser: true,
            grok_home,
        }
    }
}

/// Build the complete router. A single state contains both dashboard data and
/// Leptos options, allowing Axum to extract either through `FromRef`.
pub async fn build_router(store: Arc<DashboardStore>) -> Result<Router> {
    // This dashboard is embedded SSR, not a cargo-leptos application. Explicit
    // options avoid environment warnings and disable the development reload
    // client while keeping the fallback static root away from the working tree.
    let leptos_options = LeptosOptions::builder()
        .output_name(env!("CARGO_PKG_NAME"))
        .site_root("target/site")
        .env(Env::PROD)
        .build();
    Ok(build_router_with_options(store, leptos_options))
}

pub fn build_router_with_options(
    store: Arc<DashboardStore>,
    leptos_options: LeptosOptions,
) -> Router {
    let state = AppState {
        leptos_options: leptos_options.clone(),
        store: store.clone(),
    };
    let routes = generate_route_list(App);
    let provide_store = {
        let store = store.clone();
        move || provide_context(store.clone())
    };

    Router::new()
        .route("/api/sessions", get(api::list_sessions))
        .route("/api/sessions/{id}", get(api::get_session))
        .route("/api/sessions/{id}/timeline", get(api::get_timeline))
        .route("/api/sessions/{id}/chat", get(api::get_chat_history))
        .route("/api/sessions/{id}/charts", get(api::get_session_charts))
        .route("/api/sessions/{id}/events", get(api::live_events))
        .route("/api/metrics/server", get(api::get_server_metrics))
        .route("/api/metrics/resources", get(api::get_resource_metrics))
        .route("/api/logs", get(api::get_logs))
        .leptos_routes_with_context(&state, routes, provide_store.clone(), {
            let leptos_options = leptos_options.clone();
            move || shell(leptos_options.clone())
        })
        .fallback(leptos_axum::file_and_error_handler_with_context::<
            AppState,
            _,
        >(provide_store, shell))
        .with_state(state)
}

/// Start the dashboard. Remote binding is intentionally rejected: session
/// prompts, tool names and local paths are private machine data.
pub async fn serve(config: DashboardServerConfig) -> Result<()> {
    if !config.bind.ip().is_loopback() {
        bail!(
            "dashboard must bind to a loopback address (received {})",
            config.bind.ip()
        );
    }

    let store = Arc::new(DashboardStore::new(config.grok_home));
    if let Err(error) = store.refresh_active_sessions().await {
        tracing::warn!(%error, "unable to load active sessions; continuing with stored sessions");
    }
    let app = build_router(store).await?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}");
    tracing::info!(%url, "Hyper dashboard listening");

    if config.open_browser {
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = webbrowser::open(&url) {
                tracing::warn!(%error, "failed to open dashboard browser");
            }
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn shell(_options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <meta name="color-scheme" content="dark"/>
                <title>"Hyper Observability"</title>
                <style>{DASHBOARD_CSS}</style>
            </head>
            <body><App/></body>
        </html>
    }
}

const DASHBOARD_CSS: &str = r#"
:root {
  --bg: #090d13; --panel: #111821; --panel-2: #171f2a; --line: #273242;
  --text: #e7edf5; --muted: #8b9aad; --blue: #57a6ff; --green: #39d98a;
  --yellow: #f2c94c; --red: #ff6b6b; --purple: #b28dff;
}
* { box-sizing: border-box; }
body { margin: 0; color: var(--text); background: var(--bg); font-family: Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
a { color: var(--blue); text-decoration: none; } a:hover { text-decoration: underline; }
.container { max-width: 1440px; margin: 0 auto; padding: 24px; }
.header { display: flex; justify-content: space-between; align-items: center; gap: 24px; margin-bottom: 28px; border-bottom: 1px solid var(--line); padding-bottom: 18px; }
.brand { display: flex; align-items: baseline; gap: 10px; } .brand h1 { margin: 0; font-size: 22px; } .brand span { color: var(--muted); font-size: 12px; }
.nav { display: flex; flex-wrap: wrap; gap: 16px; } .nav a { color: var(--muted); font-size: 14px; } .nav a:hover { color: var(--text); }
h2 { font-size: 22px; margin: 0 0 20px; } h3 { font-size: 15px; margin: 0 0 12px; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); gap: 14px; margin-bottom: 24px; }
.card { display: block; background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 16px; margin-bottom: 16px; overflow: hidden; }
a.card:hover { border-color: #3e526d; text-decoration: none; }
.metrics { display: flex; gap: 22px; flex-wrap: wrap; } .metric { min-width: 90px; display: flex; flex-direction: column; gap: 3px; }
.metric-value { font-size: 22px; font-weight: 650; color: var(--text); } .metric-label { font-size: 11px; color: var(--muted); text-transform: uppercase; letter-spacing: .06em; }
.muted { color: var(--muted); } .mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
table { width: 100%; border-collapse: collapse; font-size: 13px; } th, td { padding: 11px 10px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }
th { position: sticky; top: 0; color: var(--muted); background: var(--panel); font-size: 11px; text-transform: uppercase; letter-spacing: .05em; }
.table-wrap { overflow: auto; max-height: 70vh; }
.badge { display: inline-flex; align-items: center; padding: 3px 8px; border-radius: 999px; font-size: 11px; background: #273242; color: var(--muted); }
.badge.active { background: #143d2b; color: var(--green); } .badge.error { background: #451f27; color: var(--red); }
.tabs { display: flex; gap: 14px; margin: -6px 0 20px; flex-wrap: wrap; }
.timeline { display: flex; flex-direction: column; gap: 7px; } .timeline-event { display: grid; grid-template-columns: 105px 190px 1fr auto; gap: 10px; padding: 9px 12px; border-left: 3px solid var(--line); background: var(--panel); border-radius: 4px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
.timeline-event.tool { border-left-color: var(--blue); } .timeline-event.turn { border-left-color: var(--green); } .timeline-event.error { border-left-color: var(--red); }
.chat-message { white-space: pre-wrap; overflow-wrap: anywhere; } .chat-role { color: var(--purple); font-weight: 650; text-transform: uppercase; font-size: 11px; letter-spacing: .06em; }
.log-row { display: grid; grid-template-columns: 180px 60px 85px 1fr; gap: 10px; padding: 7px 9px; border-bottom: 1px solid var(--line); font: 12px ui-monospace, SFMono-Regular, Menlo, monospace; }
.log-row.error, .log-row.warn { color: var(--red); }
.progress { height: 10px; background: #202a37; border-radius: 999px; overflow: hidden; } .progress > span { display: block; height: 100%; background: linear-gradient(90deg, var(--blue), var(--purple)); }
.chart { display: flex; flex-direction: column; gap: 9px; } .bar-row { display: grid; grid-template-columns: minmax(120px, 240px) 1fr 70px; gap: 10px; align-items: center; font-size: 12px; } .bar { height: 12px; background: #202a37; border-radius: 3px; overflow: hidden; } .bar span { display: block; height: 100%; background: var(--blue); }
.filters { display: flex; gap: 10px; flex-wrap: wrap; margin-bottom: 16px; } input, select { background: var(--panel-2); color: var(--text); border: 1px solid var(--line); border-radius: 6px; padding: 8px 10px; }
.empty { padding: 48px; text-align: center; color: var(--muted); }
.error-box { border: 1px solid #67303b; background: #2b171c; color: #ff9b9b; padding: 12px; border-radius: 8px; }
@media (max-width: 760px) { .container { padding: 14px; } .header { align-items: flex-start; flex-direction: column; } .timeline-event { grid-template-columns: 90px 1fr; } .log-row { grid-template-columns: 1fr; } }
"#;
