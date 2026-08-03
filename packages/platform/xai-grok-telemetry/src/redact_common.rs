//! Redaction helpers shared by the **internal** OTLP span pipeline
//! ([`crate::otel_layer`]) and the **external** customer-collector pipeline
//! ([`crate::external`]).
//!
//! Both pipelines are authoritative privacy chokepoints (see the crate
//! `AGENTS.md`); these helpers are the string-level scrubbing primitives they
//! share. Changes here affect every byte that leaves the process on either
//! pipeline.

use std::borrow::Cow;

/// Secret-shape then user-path scrub. Returns `Some` only when the input
/// changed (owned, so callers can overwrite in place).
pub(crate) fn redact_owned(input: &str) -> Option<String> {
    let secrets = xai_grok_secrets::redact_secrets(input);
    match xai_grok_secrets::redact_user_paths(secrets.as_ref()) {
        Cow::Owned(paths) => Some(paths),
        Cow::Borrowed(_) => match secrets {
            Cow::Owned(s) => Some(s),
            Cow::Borrowed(_) => None,
        },
    }
}

/// Scrub a string, returning the (possibly unchanged) owned value.
pub(crate) fn redact_to_owned(input: &str) -> String {
    redact_owned(input).unwrap_or_else(|| input.to_owned())
}

/// Reduce a URL to `scheme://host[:port]` — its path/query can carry user
/// content. Unparseable values are returned unchanged (callers pass the result
/// through the secret scrubber).
pub(crate) fn url_origin(value: &str) -> Cow<'_, str> {
    if let Ok(url) = url::Url::parse(value)
        && let Some(host) = url.host_str()
    {
        let origin = match url.port() {
            Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
            None => format!("{}://{}", url.scheme(), host),
        };
        return Cow::Owned(origin);
    }
    Cow::Borrowed(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_owned_scrubs_secret_shapes() {
        let out = redact_owned("key sk-CANARYabcdefghij1234567890 end")
            .expect("secret must trigger a rewrite");
        assert!(!out.contains("CANARY"), "secret survived: {out}");
    }

    #[test]
    fn redact_owned_returns_none_when_clean() {
        assert_eq!(redact_owned("no secrets here"), None);
    }

    #[test]
    fn url_origin_drops_path_and_query() {
        let origin = url_origin("https://collector.corp.example:4318/v1/logs?token=CANARY");
        assert_eq!(origin, "https://collector.corp.example:4318");
    }

    #[test]
    fn url_origin_passes_unparseable_through() {
        assert_eq!(url_origin("not a url"), "not a url");
    }
}
