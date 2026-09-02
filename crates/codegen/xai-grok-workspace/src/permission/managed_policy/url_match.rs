//! URL, argv, and name-text matching for policy entries: allow matching is a
//! positive grant (scheme/host/port/path matched separately, fail closed);
//! deny matching is host-normalized and scheme/port-agnostic (never fail
//! open); plus the unmatchable-pattern lints that warn admins.

use tracing::warn;

/// Allow matching compares schemes literally ([`AllowUrlMatcher`]), so a
/// pattern with no scheme or a glob in the scheme (`*://host/*`,
/// `*.corp.com/*`) can never match a runtime URL. That fails closed, but a
/// fleet policy written this way silently loses its grants — tell the admin.
pub(super) fn warn_on_unmatchable_allow_url(pattern: &str) {
    // Lint the same parts the matcher compiles — a warn computed from a
    // divergent parse would cry "can never match" on shapes the matcher
    // handles, training admins to ignore load-bearing warnings.
    let parts = split_allow_pattern(pattern);
    match parts.scheme {
        None => {
            warn!(
                pattern,
                "allowedMcpServers serverUrl has no scheme; schemes match literally, so this entry can never match — write e.g. https://{pattern}"
            );
            return;
        }
        Some(scheme) if scheme.contains(['*', '?', '[']) => {
            warn!(
                pattern,
                "allowedMcpServers serverUrl has a glob in its scheme; schemes match literally, so this entry can never match — write the scheme out (e.g. https://)"
            );
            return;
        }
        Some(_) => {}
    }
    if parts.bracketed_ipv6 {
        // Bracketed IPv6 compares by parsed address — any spelling of the
        // same address matches. Its port still matches as a literal number.
        let suffix = parts.authority.split_once(']').map_or("", |(_, rest)| rest);
        let port_ok = suffix.is_empty()
            || suffix
                .strip_prefix(':')
                .is_some_and(|p| p.parse::<u16>().is_ok());
        if !port_ok {
            warn!(
                pattern,
                "allowedMcpServers serverUrl port is not a number; ports match literally, so this entry can never match"
            );
        }
        return;
    }
    // Trailing dots trim like canonicalization, so a shape the matcher
    // already handles stays silent.
    let host = parts.host.trim_end_matches('.');
    // Ports are literal numbers; a glob or non-numeric port grants nothing.
    if !host.contains(':')
        && let Some(p) = parts.port
        && p.parse::<u16>().is_err()
    {
        warn!(
            pattern,
            "allowedMcpServers serverUrl port is not a number; ports match literally, so this entry can never match"
        );
        return;
    }
    if host.is_empty() {
        warn!(
            pattern,
            "allowedMcpServers serverUrl has no host; this entry can never match"
        );
    } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        // The matcher's host text works iff it equals the canonical
        // spelling the runtime side reports (`…::1:443` with a
        // canonical address + port works; `2001:0db8::1:443` not).
        if v6.to_string() != host {
            warn!(
                pattern,
                canonical = %v6,
                "allowedMcpServers serverUrl IPv6 host is not in canonical form and can never match — write the canonical spelling or bracket the address"
            );
        }
    } else if host.contains(':') && !host.contains(['*', '?', '[']) {
        // A colon-bearing LITERAL that isn't a valid address (the
        // port split ate part of an unbracketed IPv6); a glob host
        // like `2001:db8*` can still match and stays silent.
        warn!(
            pattern,
            "allowedMcpServers serverUrl IPv6 host must be bracketed; as written this entry can never match"
        );
    } else if let Some(ip) = parse_ip_host(host)
        && ip.to_string() != host
    {
        // Allow hosts glob-match the canonical runtime host, so an
        // IP in a non-canonical spelling (`127.1`, `0xa9fea9fe`)
        // can never match its connect target.
        warn!(
            pattern,
            canonical = %ip,
            "allowedMcpServers serverUrl IP host is not in canonical form and can never match — write the canonical spelling"
        );
    } else if glob::Pattern::new(&canonicalize_pattern_host(host)).is_err() {
        warn!(
            pattern,
            "allowedMcpServers serverUrl host glob does not compile; this entry can never match"
        );
    } else {
        warn_on_dead_unicode_glob_label(pattern, host, "allowedMcpServers");
    }
}

/// A deny entry that can never match is silent zero enforcement — tell the
/// admin. Covers a host-less pattern, a host glob that doesn't compile
/// (matching nothing, since no parseable runtime host contains `[`), and a
/// label mixing Unicode with glob chars.
pub(super) fn warn_on_unmatchable_deny_url(pattern: &str) {
    let (host, _) = split_host_path(pattern);
    match host {
        None => warn!(
            pattern,
            "deniedMcpServers serverUrl has no host; this entry can never match"
        ),
        Some(host) => {
            if glob::Pattern::new(&canonicalize_pattern_host(&host)).is_err() {
                warn!(
                    pattern,
                    "deniedMcpServers serverUrl host glob does not compile; the entry matches nothing"
                );
            }
            warn_on_dead_unicode_glob_label(pattern, &host, "deniedMcpServers");
        }
    }
}

/// A host label mixing non-ASCII with glob metacharacters can never match:
/// runtime hosts are punycoded, and a partial label can't be.
fn warn_on_dead_unicode_glob_label(pattern: &str, host: &str, key: &str) {
    if host
        .split('.')
        .any(|label| !label.is_ascii() && label.contains(['*', '?', '[']))
    {
        warn!(
            pattern,
            "{key} serverUrl mixes Unicode and glob characters in one host label; runtime hosts are punycoded, so this entry can never match"
        );
    }
}

/// Exact `argv == [command, args...]` (Claude semantics — no partial match).
pub(super) fn argv_matches(argv: &[String], command: &std::path::Path, args: &[String]) -> bool {
    let Some((first, rest)) = argv.split_first() else {
        return false;
    };
    *first == command.to_string_lossy() && rest == args
}

/// The transport-agnostic config name of an MCP server.
pub(super) fn mcp_server_name(server: &agent_client_protocol::McpServer) -> &str {
    match server {
        agent_client_protocol::McpServer::Http(http) => &http.name,
        agent_client_protocol::McpServer::Sse(sse) => &sse.name,
        agent_client_protocol::McpServer::Stdio(stdio) => &stdio.name,
        // `McpServer` is #[non_exhaustive] as of ACP 0.10; an unknown
        // transport has no name to match, so it never matches a policy entry.
        _ => "",
    }
}

/// Glob options for hosts and DENY paths: case-insensitive (hosts are, and
/// an insensitive deny only over-blocks), `*` spans `/` — path scoping is
/// enforced by the authority/path split, not by the glob.
const POLICY_GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// Glob options for ALLOW paths: case-sensitive — URL paths name
/// case-sensitive resources, and an allow match is a positive grant, so
/// `/mcp/*` must not grant `/MCP/*`.
const ALLOW_PATH_GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// Split `authority/path…` at the first `/` into `(authority, path)`.
fn split_authority_path(s: &str) -> (&str, &str) {
    match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// A runtime MCP URL reduced to its connect-time components by the WHATWG
/// parser reqwest uses — hand-parsing diverges from the connect target,
/// which is the gap behind every dodge in this class (`\` authority ends,
/// `%2e%2e` segments, userinfo, default ports, alternate IP spellings).
/// Patterns must NOT use this: they may hold globs the parser rejects.
struct RuntimeUrl {
    scheme: String,
    host: url::Host<String>,
    /// `None` when elided or equal to the scheme default, like the client.
    port: Option<u16>,
    /// WHATWG-normalized: never empty, dot segments resolved, no query.
    path: String,
}

impl RuntimeUrl {
    fn parse(url: &str) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host()?.to_owned();
        Some(Self {
            scheme: parsed.scheme().to_string(),
            host,
            port: parsed.port(),
            path: decode_unreserved_escapes(parsed.path()),
        })
    }

    /// Canonical bare host (lowercase, trailing dot stripped, unbracketed
    /// IPv6) — the same shape [`split_host_path`] yields for patterns.
    fn host_text(&self) -> String {
        match &self.host {
            url::Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
            url::Host::Ipv4(a) => a.to_string(),
            url::Host::Ipv6(a) => a.to_string(),
        }
    }

    /// Canonical `host[:port]` for allow matching; IPv6 stays bracketed so it
    /// lines up with `[...]` patterns.
    fn authority(&self) -> String {
        let host = match &self.host {
            url::Host::Ipv6(a) => format!("[{a}]"),
            _ => self.host_text(),
        };
        match self.port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        }
    }
}

/// An ALLOW pattern, compiled once at construction. Scheme, host, port, and
/// path match separately so a wildcard can't cross a component boundary (an
/// allow is a positive grant under the lockdown); scheme and port stay
/// literal, except a scheme-default `:443`/`:80` matches the port-less
/// spelling. The URL side parses as the client connects ([`RuntimeUrl`]);
/// unparseable = no grant. Deny matching must NOT reuse this — see
/// [`DenyUrlMatcher`].
#[derive(Debug, Clone)]
pub(super) struct AllowUrlMatcher(Option<CompiledAllow>);

#[derive(Debug, Clone)]
struct CompiledAllow {
    /// Literal scheme; a glob scheme never equals a runtime scheme (warned
    /// at parse).
    scheme: String,
    authority: AllowAuthority,
    path: AllowPath,
}

#[derive(Debug, Clone)]
enum AllowAuthority {
    /// A bracketed IPv6 PATTERN isn't a valid glob (`[` opens a class):
    /// compare parsed addresses, ports literal, so alternate spellings can't
    /// dodge. Kept textual: the scheme-default port strip depends on the
    /// runtime scheme.
    BracketedIpv6(String),
    /// Host and port match separately so a trailing host wildcard can't
    /// absorb a port constraint. No pattern port = scheme-default only; an
    /// explicit port matches the effective connect port either spelling.
    HostPort {
        /// Canonicalized host glob; `None` (glob didn't compile) grants
        /// nothing (fail closed).
        host: Option<glob::Pattern>,
        port: Option<String>,
    },
}

/// An ALLOW pattern split into the parts [`AllowUrlMatcher::new`] compiles.
/// [`warn_on_unmatchable_allow_url`] lints the SAME parts, so the warnings
/// can't drift from the matcher.
struct AllowPatternParts<'a> {
    scheme: Option<&'a str>,
    /// Authority with userinfo dropped, like the connect-time parser —
    /// before bracket detection, so `token@[::1]` still reads as IPv6.
    authority: &'a str,
    /// The authority is a bracketed IPv6 literal — compared by parsed
    /// address, never as a glob (a leading `[` whose content isn't an
    /// address opens a glob character class and splits as `host`/`port`).
    bracketed_ipv6: bool,
    /// Host half of the authority (before the last `:`). Equal to
    /// `authority` when `bracketed_ipv6` (the port stays inside it).
    host: &'a str,
    /// Port half of the authority; `None` when `bracketed_ipv6`.
    port: Option<&'a str>,
    path: &'a str,
}

fn split_allow_pattern(pattern: &str) -> AllowPatternParts<'_> {
    let (scheme, rest) = split_scheme(pattern);
    let (authority, path) = split_authority_path(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let bracketed_ipv6 = is_bracketed_ipv6(authority);
    let (host, port) = if bracketed_ipv6 {
        (authority, None)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    AllowPatternParts {
        scheme,
        authority,
        bracketed_ipv6,
        host,
        port,
        path,
    }
}

impl AllowUrlMatcher {
    pub(super) fn new(pattern: &str) -> Self {
        let parts = split_allow_pattern(pattern);
        let Some(scheme) = parts.scheme else {
            // Scheme-less patterns never match (warned at parse).
            return Self(None);
        };
        let authority = if parts.bracketed_ipv6 {
            AllowAuthority::BracketedIpv6(parts.authority.to_string())
        } else {
            AllowAuthority::HostPort {
                host: glob::Pattern::new(&canonicalize_pattern_host(parts.host)).ok(),
                port: parts.port.map(str::to_string),
            }
        };
        Self(Some(CompiledAllow {
            scheme: scheme.to_string(),
            authority,
            path: AllowPath::new(&canonicalize_pattern_path(parts.path)),
        }))
    }

    pub(super) fn matches(&self, url: &str) -> bool {
        let Some(compiled) = &self.0 else {
            return false;
        };
        let Some(runtime) = RuntimeUrl::parse(url) else {
            return false;
        };
        if !compiled.scheme.eq_ignore_ascii_case(&runtime.scheme) {
            return false;
        }
        match &compiled.authority {
            AllowAuthority::BracketedIpv6(pat_authority) => {
                let pat_authority = strip_pattern_default_port(pat_authority, &runtime.scheme);
                if !ipv6_authorities_equal(pat_authority, &runtime.authority()) {
                    return false;
                }
            }
            AllowAuthority::HostPort { host, port } => {
                if !host
                    .as_ref()
                    .is_some_and(|h| h.matches_with(&runtime.host_text(), POLICY_GLOB_OPTS))
                {
                    return false;
                }
                // Ports are literal numbers — a glob or non-numeric port
                // grants nothing (fail closed), and leading zeros compare
                // numerically.
                let port_ok = match (port.as_deref(), runtime.port) {
                    (None, None) => true,
                    (None, Some(_)) => false,
                    (Some(p), Some(port)) => p.parse::<u16>() == Ok(port),
                    (Some(p), None) => {
                        scheme_default_port(&runtime.scheme).is_some_and(|default| {
                            p.parse::<u16>().ok() == default.parse::<u16>().ok()
                        })
                    }
                };
                if !port_ok {
                    return false;
                }
            }
        }
        compiled.path.matches(&runtime.path)
    }
}

/// Canonicalize a pattern host like the WHATWG parser: IDNA/punycode
/// (`bücher.example` → `xn--bcher-kva.example`), percent-decoding
/// (`%61dmin.example` → `admin.example`), lowercase, trailing dot dropped.
/// Per label; glob labels stay literal.
fn canonicalize_pattern_host(host: &str) -> String {
    let host = host.trim_end_matches('.');
    if host.is_ascii() && !host.contains('%') {
        return host.to_ascii_lowercase();
    }
    host.split('.')
        .map(|label| {
            if label.contains(['*', '?', '[']) || (label.is_ascii() && !label.contains('%')) {
                label.to_ascii_lowercase()
            } else {
                match url::Host::parse(label) {
                    Ok(url::Host::Domain(canonical)) => canonical,
                    _ => label.to_ascii_lowercase(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Decode percent escapes of UNRESERVED bytes (RFC 3986 alphanumeric +
/// `-._~`), which the WHATWG parser leaves as-is — otherwise `/%61dmin/x`
/// dodges a `/admin/*` deny. Reserved escapes (`%2F`) stay encoded (decoding
/// them would change the path structure) but normalize to uppercase hex:
/// the parser preserves pre-existing escape case, and allow paths match
/// case-sensitively, so `%2f` and `%2F` must compare equal. Escaped dot
/// segments are already resolved on the runtime side, so a remaining `%2e`
/// can't re-create one.
fn decode_unreserved_escapes(path: &str) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            let decoded = hi * 16 + lo;
            if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
                out.push(decoded);
            } else {
                out.push(b'%');
                out.push(bytes[i + 1].to_ascii_uppercase());
                out.push(bytes[i + 2].to_ascii_uppercase());
            }
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Only ASCII bytes were substituted, so the result stays valid UTF-8.
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve literal `.`/`..` segments in a PATTERN path the way the WHATWG
/// parser resolves the runtime path, so a deny spelled `/x/../admin/*`
/// scopes to `/admin/*` instead of never matching (the runtime side always
/// arrives resolved). `..` clamps at root; a trailing dot segment keeps the
/// directory slash, like the serializer. A glob counts as one segment, so
/// `..` pops it whole.
fn resolve_pattern_dot_segments(path: &str) -> String {
    if !path.contains("/.") {
        return path.to_string();
    }
    let ends_dir = matches!(path.rsplit('/').next(), Some(".") | Some(".."));
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s),
        }
    }
    let mut joined = out.join("/");
    if joined.is_empty() {
        joined.push('/');
    }
    if ends_dir && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

/// Percent-encode a pattern path the way the WHATWG serializer emits
/// runtime paths, so a Unicode policy path (`/café/*`) matches its connect
/// spelling (`/caf%C3%A9/*`); unreserved escapes decode (kept escapes
/// uppercase, both sides) and dot segments resolve first. `?` stays literal
/// (glob metachar; a runtime path can't contain one).
fn canonicalize_pattern_path(path: &str) -> String {
    let path = resolve_pattern_dot_segments(&decode_unreserved_escapes(path));
    fn needs_encoding(b: u8) -> bool {
        !b.is_ascii()
            || b < 0x20
            || matches!(
                b,
                b' ' | b'"' | b'#' | b'<' | b'>' | b'`' | b'{' | b'}' | 0x7f
            )
    }
    if !path.bytes().any(needs_encoding) {
        return path;
    }
    let hex = |n: u8| {
        char::from_digit(n as u32, 16)
            .unwrap_or('0')
            .to_ascii_uppercase()
    };
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if needs_encoding(b) {
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 0x0f));
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Default port the WHATWG parser elides for a scheme; `None` for unknown
/// schemes (their pattern ports stay literal, failing closed on a mismatch).
fn scheme_default_port(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        _ => None,
    }
}

/// Strip an explicit scheme-default port (numerically, so `:0443` counts)
/// from a bracketed-IPv6 pattern authority — the WHATWG parser elides it on
/// the URL side, and both spellings name the same connect target. The `]`
/// guard keeps a port-less address ending in a default-port hextet intact.
fn strip_pattern_default_port<'a>(authority: &'a str, scheme: &str) -> &'a str {
    let Some(default) = scheme_default_port(scheme).and_then(|d| d.parse::<u16>().ok()) else {
        return authority;
    };
    match authority.rsplit_once(':') {
        Some((host, port)) if host.ends_with(']') && port.parse::<u16>() == Ok(default) => host,
        _ => authority,
    }
}

/// Allow-side path match: an authority-only pattern matches only the root
/// path; `/*` spans nested segments. The URL path arrives WHATWG-normalized,
/// so the glob sees what the server will actually receive.
#[derive(Debug, Clone)]
enum AllowPath {
    /// Empty or `/` pattern path: matches only the root path.
    RootOnly,
    Glob(glob::Pattern),
    /// Path glob didn't compile — grants nothing (fail closed).
    Never,
}

impl AllowPath {
    /// `canonical` must be [`canonicalize_pattern_path`] output.
    fn new(canonical: &str) -> Self {
        if canonical.is_empty() || canonical == "/" {
            return Self::RootOnly;
        }
        match glob::Pattern::new(canonical) {
            Ok(p) => Self::Glob(p),
            Err(_) => Self::Never,
        }
    }

    fn matches(&self, url_path: &str) -> bool {
        match self {
            Self::RootOnly => url_path == "/",
            Self::Glob(p) => p.matches_with(url_path, ALLOW_PATH_GLOB_OPTS),
            Self::Never => false,
        }
    }
}

/// True when a pattern authority is a bracketed IPv6 literal, `[v6]` or
/// `[v6]:port`. A leading `[` whose content doesn't parse as an address
/// opens a glob character class instead (`[ab]host.example`).
fn is_bracketed_ipv6(authority: &str) -> bool {
    authority
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(addr, _)| addr.parse::<std::net::Ipv6Addr>().is_ok())
}

/// Compare two bracketed-IPv6 authorities by parsed address (ports numeric
/// when both parse, so `:0443` ≡ `:443`), so alternate spellings of one
/// address (`[2001:0db8::1]`, `[2001:db8:0:0:0:0:0:1]`) can't dodge a
/// policy entry. When either side isn't a parseable bracketed literal, fall
/// back to the literal comparison (an imprecise allow only over-blocks).
fn ipv6_authorities_equal(a: &str, b: &str) -> bool {
    fn parse(authority: &str) -> Option<(std::net::Ipv6Addr, &str)> {
        let rest = authority.strip_prefix('[')?;
        let end = rest.find(']')?;
        let addr: std::net::Ipv6Addr = rest[..end].parse().ok()?;
        Some((addr, &rest[end + 1..]))
    }
    fn ports_equal(a: &str, b: &str) -> bool {
        match (
            a.strip_prefix(':').map(str::parse::<u16>),
            b.strip_prefix(':').map(str::parse::<u16>),
        ) {
            (Some(Ok(pa)), Some(Ok(pb))) => pa == pb,
            _ => a == b,
        }
    }
    match (parse(a), parse(b)) {
        (Some((addr_a, port_a)), Some((addr_b, port_b))) => {
            addr_a == addr_b && ports_equal(port_a, port_b)
        }
        _ => a.eq_ignore_ascii_case(b),
    }
}

/// Split `scheme://rest` into `(Some(scheme), rest)`, or `(None, s)`.
fn split_scheme(s: &str) -> (Option<&str>, &str) {
    match s.find("://") {
        Some(i) => (Some(&s[..i]), &s[i + 3..]),
        None => (None, s),
    }
}

/// A DENY pattern, compiled once at construction. Host-normalized and
/// scheme/port-agnostic — deliberately asymmetric with [`AllowUrlMatcher`]:
/// an imprecise allow only over-blocks, but a deny must never fail open, so
/// a `host` / `scheme://host/*` entry blocks that host on ANY scheme, port,
/// and path, and a URL [`RuntimeUrl`] rejects is denied outright.
#[derive(Debug, Clone)]
pub(super) struct DenyUrlMatcher(Option<CompiledDeny>);

#[derive(Debug, Clone)]
struct CompiledDeny {
    /// The (lowercased) pattern host parsed as an IP, when it is one:
    /// IP-named entries compare parsed addresses so alternate spellings of
    /// one connect target (`0xa9fea9fe`, `127.1`) are still denied; globs
    /// never parse as an IP and go through `host`.
    ip: Option<std::net::IpAddr>,
    host: DenyHost,
    path: DenyPath,
    /// Original pattern text for the broken-path-glob warn.
    pattern: String,
}

#[derive(Debug, Clone)]
enum DenyHost {
    Glob(glob::Pattern),
    /// An invalid deny HOST glob still denies by literal comparison of the
    /// canonicalized host.
    Literal(String),
}

#[derive(Debug, Clone)]
enum DenyPath {
    /// A host-only pattern blocks every path on that host — including every
    /// spelling whose CANONICAL path is `/` (`https://host/`, `/.`,
    /// `/mcp/..`), all WHATWG-equal to the pathless form.
    HostOnly,
    /// Glob the canonical pattern path against the WHATWG-normalized URL
    /// path, so `/admin/*` can't be dodged by spelling the URL
    /// `/mcp/../admin/x`. Both sides collapse empty segments
    /// ([`collapse_empty_segments`]) so `//admin/x` can't dodge either.
    Glob(glob::Pattern),
    /// Broken path glob (rendered compile error): once the host matches,
    /// deny the whole host — never fail open.
    Broken(String),
}

impl DenyUrlMatcher {
    pub(super) fn new(pattern: &str) -> Self {
        let (Some(pat_host), pat_path) = split_host_path(pattern) else {
            return Self(None);
        };
        let canonical_host = canonicalize_pattern_host(&pat_host);
        let host = match glob::Pattern::new(&canonical_host) {
            Ok(p) => DenyHost::Glob(p),
            Err(_) => DenyHost::Literal(canonical_host),
        };
        let pat_path = collapse_empty_segments(&canonicalize_pattern_path(&pat_path));
        let path = if pat_path.is_empty() || pat_path == "/" {
            DenyPath::HostOnly
        } else {
            match glob::Pattern::new(&pat_path) {
                Ok(p) => DenyPath::Glob(p),
                Err(e) => DenyPath::Broken(e.to_string()),
            }
        };
        Self(Some(CompiledDeny {
            ip: parse_ip_host(&pat_host),
            host,
            path,
            pattern: pattern.to_string(),
        }))
    }

    pub(super) fn matches(&self, url: &str) -> bool {
        let Some(compiled) = &self.0 else {
            return false;
        };
        let Some(runtime) = RuntimeUrl::parse(url) else {
            return true;
        };
        let url_host = runtime.host_text();
        if let (Some(pat_ip), Some(url_ip)) = (compiled.ip, parse_ip_host(&url_host)) {
            if pat_ip != url_ip {
                return false;
            }
        } else {
            let host_matches = match &compiled.host {
                DenyHost::Glob(p) => p.matches_with(&url_host, POLICY_GLOB_OPTS),
                DenyHost::Literal(canonical) => canonical.eq_ignore_ascii_case(&url_host),
            };
            if !host_matches {
                return false;
            }
        }
        match &compiled.path {
            DenyPath::HostOnly => true,
            DenyPath::Glob(p) => {
                p.matches_with(&collapse_empty_segments(&runtime.path), POLICY_GLOB_OPTS)
            }
            DenyPath::Broken(error) => {
                warn!(
                    pattern = %compiled.pattern,
                    error = %error,
                    "invalid deniedMcpServers path glob; denying every path on the matched host"
                );
                true
            }
        }
    }
}

/// Collapse empty path segments (`//` runs → `/`) on DENY paths only: many
/// servers treat `//admin` as `/admin`, so a leading `//` must not dodge a
/// path-scoped deny. Applied after dot-segment resolution on both the
/// pattern and runtime sides (so `/admin//../x` keeps resolving to
/// `/admin/x` first, like the WHATWG parser). Allow matching stays exact —
/// collapsing there would widen grants.
fn collapse_empty_segments(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    out
}

/// Parse a bare host as an IP, accepting every spelling the WHATWG parser
/// canonicalizes at connect time (hex, shortened, decimal, unbracketed
/// IPv6). IPv4-mapped IPv6 canonicalizes to the IPv4 it reaches on a
/// dual-stack socket. Domains and glob patterns return `None`.
fn parse_ip_host(host: &str) -> Option<std::net::IpAddr> {
    let addr = if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        std::net::IpAddr::V6(v6)
    } else {
        match url::Host::parse(host) {
            Ok(url::Host::Ipv4(a)) => std::net::IpAddr::V4(a),
            Ok(url::Host::Ipv6(a)) => std::net::IpAddr::V6(a),
            _ => return None,
        }
    };
    Some(addr.to_canonical())
}

/// Split a URL PATTERN into `(host, path)`, dropping scheme, userinfo, and
/// port. The host is lowercased with a trailing dot stripped; the path keeps
/// its original case and any glob metacharacters. Runtime URLs must be parsed
/// with [`RuntimeUrl`] instead — hand-splitting a runtime URL diverges from
/// the connect target.
fn split_host_path(s: &str) -> (Option<String>, String) {
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    let (authority, path) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, ""),
    };
    // Drop userinfo then the port; IPv6 keeps its colons whether bracketed
    // or not (a naive `:`-split truncates `2001:db8::1` to `2001`). A
    // decimal suffix is also a valid hextet; the `host:port` reading wins
    // that ambiguity (`…::1:443` is a copied URL's host + port, and reading
    // it as an address under-blocks the intended host). `:ffff` is a hextet.
    // A leading `[` only means IPv6 when its content parses as one —
    // otherwise it opens a glob character class (`[ab]evil.example`).
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let bracket_ipv6 = authority.strip_prefix('[').and_then(|rest| {
        let content = match rest.find(']') {
            Some(i) => &rest[..i],
            None => rest,
        };
        content
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
            .then_some(content)
    });
    let host = if let Some(content) = bracket_ipv6 {
        content
    } else if let Some((before_port, port)) = authority.rsplit_once(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok()
        && before_port.parse::<std::net::Ipv6Addr>().is_ok()
    {
        before_port
    } else if authority.parse::<std::net::Ipv6Addr>().is_ok() {
        authority
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        (None, path.to_string())
    } else {
        (Some(host), path.to_string())
    }
}
