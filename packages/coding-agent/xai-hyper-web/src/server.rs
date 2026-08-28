//! HTTP listener and routes for the web control plane (PR 1 skeleton).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::presented_token;
use crate::bind::check_bind;
use crate::tailscale;
use crate::token::{self, tokens_equal};

pub const DEFAULT_BIND: &str = "127.0.0.1:9100";

#[derive(Debug, Clone)]
pub struct WebServerConfig {
    pub bind: SocketAddr,
    pub grok_home: PathBuf,
    pub allow_remote: bool,
    pub open_browser: bool,
}

impl WebServerConfig {
    pub fn new(grok_home: PathBuf) -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("default bind is valid"),
            grok_home,
            allow_remote: false,
            open_browser: false,
        }
    }
}

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
}

impl FromRef<AppState> for Arc<String> {
    fn from_ref(state: &AppState) -> Self {
        state.token.clone()
    }
}

struct Authed;

impl<S> FromRequestParts<S> for Authed
where
    S: Send + Sync,
    Arc<String>: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let expected = Arc::<String>::from_ref(state);
        let Some(presented) = presented_token(parts) else {
            return Err(unauthorized());
        };
        if tokens_equal(&presented, expected.as_str()) {
            Ok(Authed)
        } else {
            Err(unauthorized())
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "missing or invalid token\n",
    )
        .into_response()
}

pub fn build_router(token: String) -> Router {
    let state = AppState {
        token: Arc::new(token),
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/api/status", get(status))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn index(Authed: Authed) -> impl IntoResponse {
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Hyper Web</title>
  <style>
    :root {{ color-scheme: dark; }}
    body {{ font-family: ui-sans-serif, system-ui, sans-serif; margin: 2rem; max-width: 40rem; }}
    code {{ font-size: 0.95em; }}
  </style>
</head>
<body>
  <h1>Hyper Web</h1>
  <p>Control plane is up. Chat sessions ship in a later drop.</p>
  <p>Token accepted. Version <code>{}</code>.</p>
</body>
</html>
"#,
        xai_grok_version::VERSION
    );
    Html(html)
}

#[derive(Serialize)]
struct StatusBody {
    ok: bool,
    version: &'static str,
}

async fn status(Authed: Authed) -> Json<StatusBody> {
    Json(StatusBody {
        ok: true,
        version: xai_grok_version::VERSION,
    })
}

pub async fn serve(config: WebServerConfig) -> Result<()> {
    check_bind(config.bind.ip(), config.allow_remote)?;
    let token = token::load_or_create(&config.grok_home)?;
    let app = build_router(token.clone());
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    let address = listener.local_addr()?;
    let local_url = format!("http://{address}");
    eprintln!("Hyper web listening on {local_url}");
    eprintln!("Open: {local_url}/?token={token}");
    eprintln!();
    eprintln!(
        "{}",
        tailscale::startup_hints(&local_url, tailscale::ipv4().as_deref())
    );

    if config.open_browser {
        let url = format!("{local_url}/?token={token}");
        tokio::task::spawn_blocking(move || {
            if let Err(error) = webbrowser::open(&url) {
                tracing::warn!(%error, "failed to open browser");
            }
        });
    }

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn call(uri: &str, token: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let req = builder.body(Body::empty()).unwrap();
        let res = build_router(TOKEN.to_string()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn healthz_is_open() {
        let (status, body) = call("/healthz", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn status_without_token_is_401() {
        let (status, _) = call("/api/status", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_with_wrong_token_is_401() {
        let (status, _) = call(
            "/api/status",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_with_bearer_ok() {
        let (status, body) = call("/api/status", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"ok\":true"));
    }

    #[tokio::test]
    async fn query_token_ok() {
        let req = Request::builder()
            .uri(format!("/api/status?token={TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let res = build_router(TOKEN.to_string()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cookie_token_ok() {
        let req = Request::builder()
            .uri("/api/status")
            .header(header::COOKIE, format!("hyper_web_token={TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let res = build_router(TOKEN.to_string()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn index_without_token_is_401() {
        let (status, _) = call("/", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
