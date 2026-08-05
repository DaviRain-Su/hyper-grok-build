//! Parse internal scheme URLs.

/// Supported internal schemes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalScheme {
    /// Last successful subagent output (`output.json`).
    Agent,
    /// Concise transcript for a subagent (or roster listing).
    History,
    /// Merge-conflict region (session-registered).
    Conflict,
}

/// Parsed internal URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalUrl {
    pub scheme: InternalScheme,
    /// Path after `scheme://` (may be empty for `history://` roster).
    pub rest: String,
}

/// Parse `agent://…`, `history://…`, or `conflict://…`. Returns `None` for
/// ordinary filesystem paths.
pub fn parse_internal_url(path: &str) -> Option<InternalUrl> {
    let path = path.trim();
    let (scheme, rest) = if let Some(r) = path.strip_prefix("agent://") {
        (InternalScheme::Agent, r)
    } else if let Some(r) = path.strip_prefix("history://") {
        (InternalScheme::History, r)
    } else if let Some(r) = path.strip_prefix("conflict://") {
        (InternalScheme::Conflict, r)
    } else {
        return None;
    };
    // Drop accidental leading slashes: agent:///id → id
    let rest = rest.trim_start_matches('/').to_string();
    Some(InternalUrl { scheme, rest })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_schemes() {
        let u = parse_internal_url("agent://sa-1").unwrap();
        assert_eq!(u.scheme, InternalScheme::Agent);
        assert_eq!(u.rest, "sa-1");

        let u = parse_internal_url("history://").unwrap();
        assert_eq!(u.scheme, InternalScheme::History);
        assert_eq!(u.rest, "");

        let u = parse_internal_url("conflict://2/ours").unwrap();
        assert_eq!(u.scheme, InternalScheme::Conflict);
        assert_eq!(u.rest, "2/ours");
    }

    #[test]
    fn rejects_normal_paths() {
        assert!(parse_internal_url("src/main.rs").is_none());
        assert!(parse_internal_url("/tmp/x").is_none());
        assert!(parse_internal_url("file://tmp").is_none());
    }
}
