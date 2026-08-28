//! Extract a presented token from Bearer / query / cookie.

use axum::http::request::Parts;

const COOKIE_NAME: &str = "hyper_web_token";

pub fn presented_token(parts: &Parts) -> Option<String> {
    if let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(value) = value.to_str()
    {
        let value = value.trim();
        if let Some(rest) = value.strip_prefix("Bearer ") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }

    if let Some(query) = parts.uri.query() {
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            if parts.next() == Some("token")
                && let Some(value) = parts.next()
                && !value.is_empty()
            {
                return Some(value.to_string());
            }
        }
    }

    if let Some(value) = parts.headers.get(axum::http::header::COOKIE)
        && let Ok(value) = value.to_str()
    {
        for pair in value.split(';') {
            let pair = pair.trim();
            if let Some(rest) = pair.strip_prefix(COOKIE_NAME)
                && let Some(rest) = rest.strip_prefix('=')
                && !rest.is_empty()
            {
                return Some(rest.to_string());
            }
        }
    }

    None
}
