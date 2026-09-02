//! Marketplace policy: per-source allowlists, the cross-source
//! [`MarketplacePolicy`] (strictest wins), managed marketplace pins, and the
//! canonical git-URL identity.

use super::layer::{PolicyLayerOwnership, PolicySourceAuthority};
use super::mcp::PolicySubjectOrigin;

/// Marketplace allowlist from ONE managed source.
#[derive(Debug, Clone, Default)]
pub struct MarketplaceAllowlist {
    pub allowed_urls: Vec<String>,
    pub source_path: Option<std::path::PathBuf>,
    /// Whether this source's restrictions bind grok-native marketplaces.
    pub authority: PolicySourceAuthority,
}

impl MarketplaceAllowlist {
    pub fn is_restricted(&self) -> bool {
        !self.allowed_urls.is_empty()
    }

    /// See [`McpServerAllowlist::binds`].
    fn binds(&self, origin: PolicySubjectOrigin) -> bool {
        self.authority == PolicySourceAuthority::Native || origin == PolicySubjectOrigin::Foreign
    }

    pub fn is_url_allowed(&self, url: &str) -> bool {
        if !self.is_restricted() {
            return true;
        }
        let normalized = normalize_git_url(url);
        self.allowed_urls
            .iter()
            .any(|allowed| normalize_git_url(allowed) == normalized)
    }

    pub fn block_reason(&self) -> String {
        match &self.source_path {
            Some(p) => format!("source not in strictKnownMarketplaces ({})", p.display()),
            None => "source not in strictKnownMarketplaces".to_string(),
        }
    }
}

/// Marketplace policy across all sources: a URL must pass every restricted
/// source (strictest wins).
#[derive(Debug, Clone, Default)]
pub struct MarketplacePolicy {
    pub sources: Vec<MarketplaceAllowlist>,
}

impl MarketplacePolicy {
    /// A single-source policy (test construction across crates).
    pub fn single(allowlist: MarketplaceAllowlist) -> Self {
        Self {
            sources: vec![allowlist],
        }
    }

    pub fn is_restricted(&self) -> bool {
        self.sources.iter().any(MarketplaceAllowlist::is_restricted)
    }

    /// Restriction active for a subject of `origin` (advisory strict lists
    /// don't bind grok-native marketplaces).
    pub fn is_restricted_for(&self, origin: PolicySubjectOrigin) -> bool {
        self.sources
            .iter()
            .any(|s| s.binds(origin) && s.is_restricted())
    }

    pub fn is_url_allowed(&self, url: &str, origin: PolicySubjectOrigin) -> bool {
        self.sources
            .iter()
            .filter(|s| s.binds(origin))
            .all(|s| s.is_url_allowed(url))
    }

    /// Reason `url` is blocked, attributed to the binding source that
    /// actually rejects it (falling back to the first binding restricted
    /// source when every one allows — callers only ask after a block).
    pub fn block_reason(&self, url: &str, origin: PolicySubjectOrigin) -> String {
        self.sources
            .iter()
            .find(|s| s.binds(origin) && s.is_restricted() && !s.is_url_allowed(url))
            .or_else(|| {
                self.sources
                    .iter()
                    .find(|s| s.binds(origin) && s.is_restricted())
            })
            .map(MarketplaceAllowlist::block_reason)
            .unwrap_or_else(|| "source not in strictKnownMarketplaces".to_string())
    }

    /// Fail-closed add/install gate: `Some(reason)` when restricted and
    /// `identity` isn't allowed (local paths never match — intentional).
    /// The subject of an add/install is by definition not yet grok-native, so
    /// every policy source binds — including advisory ones. The advisory
    /// carve-out covers what grok config already defines, never the
    /// acquisition of new sources.
    pub fn add_block_reason(&self, identity: &str) -> Option<String> {
        let origin = PolicySubjectOrigin::Foreign;
        (self.is_restricted_for(origin) && !self.is_url_allowed(identity, origin))
            .then(|| self.block_reason(identity, origin))
    }

    /// Union across sources — display only (matching intersects).
    pub fn allowed_urls(&self) -> Vec<String> {
        let mut out = Vec::new();
        for source in &self.sources {
            for url in &source.allowed_urls {
                if !out.contains(url) {
                    out.push(url.clone());
                }
            }
        }
        out
    }
}

/// A marketplace pinned by managed policy via `extraKnownMarketplaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedMarketplace {
    pub name: String,
    pub kind: ManagedMarketplaceKind,
    /// Ownership of the layer that provisioned the pin.
    pub ownership: PolicyLayerOwnership,
}

/// How to reach a managed marketplace. `github`+`repo` sources are
/// canonicalized to their clone URL at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedMarketplaceKind {
    Git {
        url: String,
        /// Optional branch/tag (`ref` in the Claude JSON).
        git_ref: Option<String>,
    },
    Local {
        path: String,
    },
}

/// Canonical git-URL identity for marketplace allowlist/dedup comparisons.
/// Only the scheme and authority fold case — repo paths are case-sensitive on
/// most git hosts, so lowercasing them would widen an allowlist entry to
/// sibling repos. Exactly one `.git` suffix is stripped (`repo.git.git` and
/// `repo` are different repos).
pub fn normalize_git_url(url: &str) -> String {
    let url = url.strip_suffix(".git").unwrap_or(url);
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        return format!(
            "{}://{}{path}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase()
        );
    }
    // scp-style `git@host:org/repo` — the part before `:` is user@host.
    if let Some((user_host, path)) = url.split_once(':')
        && user_host.contains('@')
    {
        return format!("{}:{path}", user_host.to_ascii_lowercase());
    }
    url.to_string()
}
