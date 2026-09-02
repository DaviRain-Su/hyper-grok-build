use super::mcp::{
    MANAGED_MCP_NAME_MAX_CHARS, MANAGED_MCP_PREFIX, mcp_name_matches, normalize_managed_name,
};
use super::parse::{McpPolicyList, parse_mcp_entry_list};
use super::*;
use crate::permission::rules::DefaultPermissionMode;
use crate::permission::types::RuleAction;
use std::path::PathBuf;
const FOREIGN: PolicySubjectOrigin = PolicySubjectOrigin::Foreign;
const NATIVE: PolicySubjectOrigin = PolicySubjectOrigin::GrokNative;
const CLAUDE_PATH: &str = "/test/managed-settings.json";
const SYS_REQ: &str = "/etc/grok/requirements.toml";
const USER_REQ: &str = "/home/u/.grok/requirements.toml";
/// HTTP server named `name` at `url`.
fn hs(name: &str, url: &str) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Http(
        agent_client_protocol::McpServerHttp::new(name, url).headers(vec![]),
    )
}
/// Stdio server named `name` running `command` (no args).
fn ss(name: &str, command: &str) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio::new(
        name,
        std::path::PathBuf::from(command),
    ))
}
/// Stdio server with args.
fn sa(name: &str, command: &str, args: &[&str]) -> agent_client_protocol::McpServer {
    agent_client_protocol::McpServer::Stdio(
        agent_client_protocol::McpServerStdio::new(name, std::path::PathBuf::from(command))
            .args(args.iter().map(|s| s.to_string()).collect()),
    )
}
/// Anonymous HTTP server for URL-only rows (the name never matters there).
fn h(url: &str) -> agent_client_protocol::McpServer {
    hs("t", url)
}
fn allowlist_from(json: serde_json::Value) -> McpServerPolicy {
    parse_managed_settings_json(&json, std::path::Path::new(CLAUDE_PATH)).mcp_allowlist
}
/// Policy with URL-pattern entries in `allowedMcpServers`.
fn allow_urls(patterns: &[&str]) -> McpServerPolicy {
    let entries: Vec<_> = patterns
        .iter()
        .map(|p| serde_json::json!({ "serverUrl": p }))
        .collect();
    allowlist_from(serde_json::json!({ "allowedMcpServers": entries }))
}
/// Policy with URL-pattern entries in `deniedMcpServers`.
fn deny_urls(patterns: &[&str]) -> McpServerPolicy {
    let entries: Vec<_> = patterns
        .iter()
        .map(|p| serde_json::json!({ "serverUrl": p }))
        .collect();
    allowlist_from(serde_json::json!({ "deniedMcpServers": entries }))
}
/// Assert `is_server_allowed` (Foreign) for each `(server, want, why)` row.
fn check_allowed(
    group: &str,
    al: &McpServerPolicy,
    rows: &[(agent_client_protocol::McpServer, bool, &str)],
) {
    for (server, want, label) in rows {
        assert_eq!(
            al.is_server_allowed(server, FOREIGN),
            *want,
            "{group} / {label}"
        );
    }
}
/// Assert `is_server_denied` (Foreign) for each `(server, want, why)` row.
fn check_denied(
    group: &str,
    al: &McpServerPolicy,
    rows: &[(agent_client_protocol::McpServer, bool, &str)],
) {
    for (server, want, label) in rows {
        assert_eq!(
            al.is_server_denied(server, FOREIGN),
            *want,
            "{group} / {label}"
        );
    }
}
/// Allow-table group: build an allow-URL policy and pin `is_server_allowed`
/// for each `(url, granted, why)` row.
fn allow_rows(group: &str, patterns: &[&str], rows: &[(&str, bool, &str)]) {
    let al = allow_urls(patterns);
    for (url, want, label) in rows {
        assert_eq!(
            al.is_server_allowed(&h(url), FOREIGN),
            *want,
            "{group} / {label}: {url}"
        );
    }
}
/// Deny-table group: build a deny-URL policy and pin `is_server_denied` for
/// each `(url, denied, why)` row, plus the allow side (for a deny-only
/// policy, allowed is exactly the complement of denied).
fn deny_rows(group: &str, patterns: &[&str], rows: &[(&str, bool, &str)]) {
    let al = deny_urls(patterns);
    for (url, denied, label) in rows {
        assert_eq!(
            al.is_server_denied(&h(url), FOREIGN),
            *denied,
            "{group} / {label}: {url}"
        );
        assert_eq!(
            al.is_server_allowed(&h(url), FOREIGN),
            !*denied,
            "{group} / {label} (allow side): {url}"
        );
    }
}
#[test]
fn parse_managed_settings_json_end_to_end() {
    let json = serde_json::json!({
        "env": {
            "DISABLE_TELEMETRY": 1,
            "DISABLE_FEEDBACK_COMMAND": 1
        },
        "permissions": {
            "disableBypassPermissionsMode": "disable",
            "deny": ["Read(**/.env*)"]
        },
        "allowedMcpServers": [
            { "serverUrl": "https://*.example.com/*" },
            { "command": "npx" }
        ],
        "strictKnownMarketplaces": [
            { "source": "git", "url": "git@github.enterprise.example:ACME/repo.git" }
        ]
    });
    let path = std::path::Path::new(CLAUDE_PATH);
    let ms = parse_managed_settings_json(&json, path);
    assert_eq!(ms.features.disable_telemetry, Some(true));
    assert_eq!(ms.features.disable_feedback, Some(true));
    assert_eq!(ms.features.disable_yolo, Some(true));
    assert!(ms.mcp_allowlist.is_restricted());
    check_allowed(
        "end-to-end MCP allowlist",
        &ms.mcp_allowlist,
        &[
            (h("https://api.example.com/mcp"), true, "allowlisted URL"),
            (h("https://evil.com/mcp"), false, "unlisted URL"),
            (
                h("https://evil.com/?x=https://fake.example.com/y"),
                false,
                "embedded URL in a query string is no bypass",
            ),
            (ss("t", "npx"), true, "allowlisted command"),
            (ss("t", "node"), false, "unlisted command"),
        ],
    );
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(
        ms.marketplace_allowlist
            .is_url_allowed("git@github.enterprise.example:ACME/repo.git", FOREIGN)
    );
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("git@evil.com:org/repo.git", FOREIGN)
    );
    assert_eq!(ms.permissions.len(), 1);
    assert_eq!(ms.permissions[0].value.action, RuleAction::Deny);
}
#[test]
fn mcp_denylist_classifies_denied_servers() {
    let al = allowlist_from(serde_json::json!({
        "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ],
        "deniedMcpServers": [ { "serverUrl": "https://blocked.example.com/*" } ]
    }));
    let denied = || hs("blocked", "https://blocked.example.com/mcp");
    let unlisted = || hs("other", "https://other.com/mcp");
    check_allowed(
        "deny vs missing-allowlist classification",
        &al,
        &[
            (denied(), false, "denied server is blocked"),
            (unlisted(), false, "unlisted server is blocked"),
        ],
    );
    check_denied(
        "deny vs missing-allowlist classification",
        &al,
        &[
            (denied(), true, "denied server classifies as denied"),
            (unlisted(), false, "blocks as missing-allowlist, not a deny"),
        ],
    );
}
/// What this table pins: ALLOW URL patterns are a positive grant — scheme,
/// host, port, and path match separately so a wildcard never crosses a
/// component boundary; scheme and port stay literal (a scheme-default port
/// matches its port-less spelling); hosts and paths canonicalize exactly like
/// the WHATWG parser the client connects with (IDNA/punycode, IP spellings,
/// userinfo, dot segments, percent escapes); anything unparseable or
/// uncompilable grants nothing (fail closed). Each group is one pattern set;
/// rows are (runtime URL, granted?, why).
#[test]
#[rustfmt::skip]
fn allow_url_matcher_semantics() {
    allow_rows(
        "host wildcard *.corp.com",
        &["https://*.corp.com/*"],
        &[
            ("https://mcp.corp.com/sse", true, "subdomain host is granted"),
            ("https://a.corp.com/x/y", true, "nested path under a subdomain"),
            ("https://sub.sub.corp.com/x", true, "any subdomain depth matches"),
            ("https://corp.com/x", false, "`*.` needs a subdomain label: no apex grant"),
            (
                "https://evil.example/a.corp.com/x",
                false,
                "`*` must not span `evil.example/a`",
            ),
            (
                "https://a.corp.com@evil.example/x",
                false,
                "userinfo decoy: connect host is evil",
            ),
            (
                "https://evil.example\\@a.corp.com/x",
                false,
                "`\\` ends the authority like connect",
            ),
            ("http://mcp.corp.com/sse", false, "scheme stays literal"),
            ("https://mcp.corp.com:8080/sse", false, "port stays literal"),
        ],
    );
    allow_rows(
        "path glob /* and scheme case",
        &["https://mcp.corp.com/*"],
        &[
            ("https://mcp.corp.com/", true, "trailing-slash spelling"),
            ("https://mcp.corp.com", true, "path-less spelling — same request as `/`"),
            ("https://mcp.corp.com/a/b", true, "nested path"),
            (
                "HTTPS://mcp.corp.com/mcp",
                true,
                "uppercase HTTPS:// parses to the same scheme",
            ),
            ("mcp.corp.com/mcp", false, "relative (unparseable) URL earns no grant"),
        ],
    );
    allow_rows(
        "dot segments resolve; allow paths are case-sensitive",
        &["https://corp.com/mcp/*"],
        &[
            (
                "https://corp.com/mcp/../admin",
                false,
                "`..` connects to /admin — outside",
            ),
            (
                "https://corp.com/mcp/%2e%2e/admin",
                false,
                "`%2e%2e` resolves like connect time",
            ),
            (
                "https://corp.com/mcp/.%2e/admin",
                false,
                "mixed literal+escaped dot resolves too",
            ),
            ("https://corp.com/mcp/./tool", true, "benign `/./` lands inside the grant"),
            ("https://corp.com/mcp/x", true, "matching-case path is granted"),
            (
                "https://corp.com/MCP/x",
                false,
                "a different-cased path is a different resource",
            ),
            ("https://CORP.com/mcp/x", true, "host case stays irrelevant"),
        ],
    );
    allow_rows(
        "explicit scheme-default port",
        &["https://mcp.corp.com:443/*"],
        &[
            (
                "https://mcp.corp.com/mcp",
                true,
                "port-less spelling names the same target",
            ),
            ("https://mcp.corp.com:443/mcp", true, "explicit :443 spelling"),
            ("https://mcp.corp.com:8080/mcp", false, "non-default port stays literal"),
        ],
    );
    allow_rows(
        "trailing host wildcard, no pattern port",
        &["https://mcp.corp.*/*"],
        &[
            ("https://mcp.corp.com/mcp", true, "host glob matches"),
            (
                "https://mcp.corp.com:8080/mcp",
                false,
                "host wildcard cannot absorb a port",
            ),
        ],
    );
    allow_rows(
        "trailing host wildcard with explicit default port",
        &["https://mcp.corp.*:443/*"],
        &[
            ("https://mcp.corp.com/mcp", true, "port-less spelling"),
            ("https://mcp.corp.com:443/mcp", true, "explicit default port"),
            ("https://mcp.corp.com:8080/mcp", false, "non-default port"),
        ],
    );
    allow_rows(
        "Unicode pattern host and path",
        &["https://bücher.example/café/*"],
        &[
            ("https://bücher.example/café/tool", true, "Unicode runtime spelling"),
            (
                "https://xn--bcher-kva.example/caf%C3%A9/tool",
                true,
                "punycode connect spelling",
            ),
            (
                "https://xn--bcher-kva.example/other/tool",
                false,
                "different path stays out",
            ),
        ],
    );
    allow_rows(
        "NFD-encoded Unicode pattern host",
        &["https://bu\u{0308}cher.example/*"],
        &[
            (
                "https://b\u{00fc}cher.example/x",
                true,
                "NFC runtime spelling of the same host",
            ),
            ("https://xn--bcher-kva.example/x", true, "punycode runtime spelling"),
        ],
    );
    allow_rows(
        "bracketed IPv6 compares by parsed address",
        &["https://[2001:db8::1]/*"],
        &[
            ("https://[2001:db8::1]/mcp", true, "literal spelling"),
            (
                "https://[2001:db8:0:0:0:0:0:1]/mcp",
                true,
                "expanded spelling of the address",
            ),
            ("https://[2001:db8::2]/mcp", false, "different address"),
            ("https://[2001:db8::1]:8080/mcp", false, "non-default port stays literal"),
        ],
    );
    allow_rows(
        "leading-zero IPv6 PATTERN spelling",
        &["https://[2001:0db8::1]/*"],
        &[("https://[2001:db8::1]/mcp", true, "still compares by parsed address")],
    );
    allow_rows(
        "IPv6 address ending in a default-port hextet",
        &["https://[2001:db8::443]/*"],
        &[
            (
                "https://[2001:db8::443]/mcp",
                true,
                "no port to strip: address stays intact",
            ),
            (
                "https://[2001:db8::443]:443/mcp",
                true,
                "explicit default port on the address",
            ),
        ],
    );
    allow_rows(
        "zero-padded default port on IPv6",
        &["https://[::1]:0443/*"],
        &[("https://[::1]/mcp", true, "strips numerically like the parser")],
    );
    allow_rows(
        "zero-padded non-default port on IPv6",
        &["https://[::1]:08080/*"],
        &[
            ("https://[::1]:8080/mcp", true, "both-explicit ports compare numerically"),
            ("https://[::1]/mcp", false, "port-less spelling is a different target"),
        ],
    );
    allow_rows(
        "host wildcard grants IP runtimes too",
        &["https://*/*"],
        &[
            ("https://[::1]/mcp", true, "IPv6 runtime"),
            ("https://10.0.0.1/mcp", true, "IPv4 runtime"),
        ],
    );
    allow_rows(
        "IPv6 zone-id URL fails closed",
        &["https://[fe80::1]/*"],
        &[
            (
                "https://[fe80::1%25eth0]/mcp",
                false,
                "zone-id URL is unparseable — no grant",
            ),
        ],
    );
    allow_rows(
        "IPv6 zone-id PATTERN can never match",
        &["https://[fe80::1%eth0]/*"],
        &[
            ("https://[fe80::1]/mcp", false, "not the plain address"),
            (
                "https://[fe80::1%25eth0]/mcp",
                false,
                "not even the zone-id URL (unparseable)",
            ),
        ],
    );
    allow_rows(
        "leading non-address [..] is a glob character class",
        &["https://[ab]host.corp.com/*"],
        &[
            ("https://ahost.corp.com/mcp", true, "class member a"),
            ("https://bhost.corp.com/mcp", true, "class member b"),
            ("https://chost.corp.com/mcp", false, "non-member c"),
        ],
    );
    allow_rows(
        "glob in the pattern port",
        &["https://mcp.corp.com:4*/*"],
        &[
            (
                "https://mcp.corp.com:443/mcp",
                false,
                "ports are literal: a glob grants nothing",
            ),
            ("https://mcp.corp.com:4000/mcp", false, "not even other 4xxx ports"),
        ],
    );
    allow_rows(
        "zero-padded pattern port on a domain host",
        &["https://mcp.corp.com:0443/*"],
        &[("https://mcp.corp.com:443/mcp", true, "leading zeros compare numerically")],
    );
    allow_rows(
        "pattern userinfo drops like the connect-time parser",
        &["https://token@mcp.corp.com/*", "https://token@[::1]/*"],
        &[
            (
                "https://mcp.corp.com/x",
                true,
                "copied token@domain still grants its host",
            ),
            ("https://[::1]/x", true, "token@[IPv6] still reads as an address"),
        ],
    );
    allow_rows(
        "escape hex case never splits one connect target",
        &["https://h.example/caf%c3%a9/*"],
        &[
            ("https://h.example/caf%C3%A9/x", true, "uppercase-hex runtime spelling"),
            ("https://h.example/caf%c3%a9/x", true, "lowercase-hex runtime spelling"),
            ("https://h.example/café/x", true, "raw Unicode runtime spelling"),
        ],
    );
    allow_rows(
        "broken allow HOST glob grants nothing",
        &["https://host[x.corp.com/*"],
        &[
            (
                "https://hostx.corp.com/mcp",
                false,
                "unclosed character class fails closed",
            ),
            ("https://host.corp.com/mcp", false, "no other corp host either"),
        ],
    );
    allow_rows(
        "double-trailing-dot host spelling",
        &["https://evil.com/*"],
        &[
            (
                "https://evil.com../x",
                true,
                "parses; trailing dots trim to the same connect host — granted, not a bypass",
            ),
        ],
    );
}
/// What this table pins: DENY URL patterns are host-normalized and
/// scheme/port-agnostic — deliberately asymmetric with allow (an imprecise
/// allow only over-blocks, but a deny must never fail open): IP-named entries
/// compare parsed addresses across every alternate spelling, hosts
/// canonicalize on both sides, a URL the connect-time parser rejects is
/// denied outright, and broken path globs deny the matched host rather than
/// disable the entry. Each group is one pattern set; rows are (runtime URL,
/// denied?, why) — the allow side is pinned as the exact complement.
#[test]
#[rustfmt::skip]
fn deny_url_matcher_semantics() {
    deny_rows(
        "host-normalized, scheme/port-agnostic",
        &["https://mcp-gateway.example.net/*"],
        &[
            ("https://mcp-gateway.example.net:443/mcp", true, "explicit default port"),
            ("http://mcp-gateway.example.net/mcp", true, "scheme swap"),
            ("https://mcp-gateway.example.net", true, "path-less host"),
            ("https://mcp-gateway.example.net./mcp", true, "trailing-dot FQDN"),
            ("https://mcp-gateway.example.net/mcp", true, "baseline spelling"),
            (
                "https://mcp-gateway.example.net/mcp?x=y",
                true,
                "query strips — no bypass",
            ),
            ("https://MCP-GATEWAY.example.net/mcp", true, "host case folds"),
            (
                "https://mcp-gateway.staging.example.net/mcp",
                false,
                "deny is host-scoped",
            ),
            ("https://other.example.com/mcp", false, "unrelated host stays"),
        ],
    );
    deny_rows(
        "path-scoped deny survives spelling dodges",
        &["https://corp.com/admin/*"],
        &[
            ("https://corp.com/mcp/../admin/x", true, "`..` connects under /admin"),
            (
                "https://corp.com/mcp/%2e%2e/admin/x",
                true,
                "`%2e%2e` resolves the same way",
            ),
            (
                "https://corp.com/admin//../x",
                true,
                "`//../x` pops only the empty segment",
            ),
            (
                "https://corp.com/%61dmin/x",
                true,
                "unreserved escape decodes (`%61` = `a`)",
            ),
            (
                "https://corp.com/a%2Fdmin/x",
                false,
                "%2F is not a separator; stays encoded",
            ),
            (
                "https://corp.com//admin/x",
                true,
                "double-slash prefix can't dodge the path glob",
            ),
            ("https://corp.com/admin///x", true, "interior empty segments collapse too"),
        ],
    );
    deny_rows(
        "empty segments collapse on the pattern side",
        &["https://corp.com//admin/*"],
        &[
            ("https://corp.com/admin/x", true, "pattern `//admin` scopes to `/admin`"),
            (
                "https://corp.com//admin/x",
                true,
                "and still catches the double-slash spelling",
            ),
        ],
    );
    deny_rows(
        "backslash authority terminator",
        &["https://evil.example/*"],
        &[("https://evil.example\\@a.corp.com/x", true, "connect host is evil.example")],
    );
    deny_rows(
        "IPv4 deny matches every connect-equal spelling",
        &["http://169.254.169.254/*"],
        &[
            ("http://0xa9fea9fe/latest/meta-data", true, "hex spelling"),
            (
                "http://[::ffff:169.254.169.254]/latest/meta-data",
                true,
                "IPv4-mapped IPv6",
            ),
            (
                "http://[::ffff:a9fe:a9fe]/latest/meta-data",
                true,
                "hex-group IPv4-mapped",
            ),
            ("http://[2001:db8::1]/mcp", false, "a genuine IPv6 address is not an alias"),
        ],
    );
    deny_rows(
        "localhost IPv4 deny",
        &["http://127.0.0.1/*"],
        &[
            ("http://127.1/mcp", true, "shortened spelling"),
            ("http://2130706433/mcp", true, "decimal spelling"),
            ("http://10.0.0.1/mcp", false, "different address untouched"),
        ],
    );
    deny_rows(
        "IPv4-mapped deny entry (mirror direction)",
        &["http://[::ffff:127.0.0.1]/*"],
        &[("http://127.0.0.1/mcp", true, "blocks the plain IPv4 spelling")],
    );
    deny_rows(
        "Unicode deny host",
        &["https://bücher.example/*"],
        &[
            ("https://bücher.example/mcp", true, "Unicode runtime spelling"),
            ("https://xn--bcher-kva.example/mcp", true, "punycode runtime spelling"),
            ("https://XN--BCHER-KVA.example/mcp", true, "mixed-case punycode folds"),
            ("https://other.example/mcp", false, "unrelated host untouched"),
        ],
    );
    deny_rows(
        "wildcard label around Unicode labels",
        &["https://*.bücher.example/*"],
        &[("https://mcp.xn--bcher-kva.example/x", true, "glob label stays a glob")],
    );
    deny_rows(
        "host wildcard *.corp.com (deny side)",
        &["https://*.corp.com/*"],
        &[
            (
                "https://corp.com/mcp",
                false,
                "as-is: `*.` needs a subdomain label, so the apex is NOT denied — an apex deny \
             must be written without the `*.`",
            ),
        ],
    );
    deny_rows(
        "unparseable runtime URLs are denied outright",
        &["https://blocked.example.com/*"],
        &[("mcp.corp.com/mcp", true, "relative URL fails closed")],
    );
    deny_rows(
        "double-trailing-dot host spelling (deny side)",
        &["https://evil.com/*"],
        &[("https://evil.com../x", true, "trims to the denied host — no dodge")],
    );
    deny_rows(
        "IPv6 zone-id URL (deny side)",
        &["https://[fe80::1]/*"],
        &[("https://[fe80::1%25eth0]/mcp", true, "unparseable — denied outright")],
    );
    deny_rows(
        "IPv6 zone-id PATTERN (deny side)",
        &["https://[fe80::1%eth0]/*"],
        &[
            ("https://[fe80::1]/mcp", false, "never matches the plain address"),
            (
                "https://[fe80::1%25eth0]/mcp",
                true,
                "still denies the unparseable zone-id URL",
            ),
        ],
    );
    deny_rows(
        "IPv6 deny matches every spelling of the address",
        &["https://[2001:db8::1]/*"],
        &[
            ("https://[2001:db8::1]/mcp", true, "literal spelling"),
            (
                "http://[2001:db8::1]:8080/mcp",
                true,
                "any scheme/port variant still denied",
            ),
            ("https://[2001:0db8::1]/mcp", true, "leading-zero spelling"),
            ("https://[2001:db8:0:0:0:0:0:1]/mcp", true, "expanded spelling"),
            ("https://[2001:db8::2]/mcp", false, "different address untouched"),
        ],
    );
    deny_rows(
        "trailing-slash deny blocks the whole host",
        &["https://blocked.example.com/"],
        &[
            ("https://blocked.example.com/mcp", true, "deep path"),
            ("https://blocked.example.com/", true, "root path"),
            ("https://ok.example.com/mcp", false, "other host untouched"),
        ],
    );
    for pattern in [
        "https://blocked.example.com/.",
        "https://blocked.example.com/mcp/..",
        "https://blocked.example.com/./",
    ] {
        deny_rows(
            pattern,
            &[pattern],
            &[
                (
                    "https://blocked.example.com/mcp",
                    true,
                    "canonical-root pattern denies host",
                ),
            ],
        );
    }
    deny_rows(
        "leading non-address [..] globs the host",
        &["https://[ab]evil.example/*"],
        &[
            ("https://aevil.example/x", true, "class member a"),
            ("https://bevil.example/x", true, "class member b"),
            ("https://cevil.example/x", false, "non-member c"),
        ],
    );
    deny_rows(
        "percent-encoded pattern host decodes",
        &["https://%61dmin.example/*"],
        &[
            ("https://admin.example/x", true, "blocks its real host"),
            ("https://badmin.example/x", false, "not a substring host"),
        ],
    );
    deny_rows(
        "pattern path dot segments resolve",
        &["https://h.example/x/../admin/*"],
        &[
            ("https://h.example/admin/secret", true, "scopes to /admin/*"),
            ("https://h.example/x/admin/secret", false, "not the unresolved spelling"),
        ],
    );
    deny_rows(
        "percent-encoded unreserved bytes in the PATTERN path",
        &["https://h.example/%61dmin/*"],
        &[("https://h.example/admin/x", true, "matches the plain runtime path")],
    );
    deny_rows(
        "invalid deny PATH glob fails closed",
        &["https://h.example/admin[x/*"],
        &[
            (
                "https://h.example/admin[x/y",
                true,
                "host matched + broken glob: deny the host",
            ),
            (
                "https://other.example/admin[x/y",
                false,
                "different host: entry does not apply",
            ),
            (
                "https://host[x/y",
                true,
                "unparseable URL denied on the unparseable branch",
            ),
        ],
    );
    deny_rows(
        "broken deny HOST glob",
        &["https://host[x/*"],
        &[
            ("https://host[x/y", true, "still denies the unparseable same-text URL"),
            (
                "https://other.example/admin",
                false,
                "stays scoped: parseable hosts untouched",
            ),
        ],
    );
    deny_rows(
        "unbracketed IPv6 deny pattern",
        &["https://2001:db8::1/*"],
        &[
            ("https://[2001:db8::1]/mcp", true, "denies the bracketed connect spelling"),
            ("https://0.0.7.209/mcp", false, "not misread as IPv4 by a first-`:` split"),
        ],
    );
    deny_rows(
        "unbracketed IPv6 with a trailing decimal group",
        &["https://2001:db8::1:443/*"],
        &[
            (
                "https://[2001:db8::1]/mcp",
                true,
                ":443 is a PORT — denies host 2001:db8::1",
            ),
            ("https://[2001:db8::1]:8080/mcp", true, "deny is port-agnostic"),
            (
                "https://[2001:db8::1:443]/mcp",
                false,
                "the different address is untouched",
            ),
        ],
    );
    deny_rows(
        "unbracketed IPv6 with a non-decimal final group",
        &["https://2001:db8::1:ffff/*"],
        &[
            (
                "https://[2001:db8::1:ffff]/mcp",
                true,
                ":ffff is a hextet, not a port — the whole string is the address",
            ),
        ],
    );
}
/// What this table pins: allow dimensions union at the production chokepoint
/// (`is_server_allowed`, where a match-guard fall-through once flipped it) —
/// a command-only allowlist restricts stdio servers, never HTTP, and vice
/// versa (same for denylists); a deny-only policy restricts, denying its
/// entries (command denies exact-string) while allowing the rest; deny beats
/// allow on every dimension; `serverName` matches transport-agnostically;
/// `serverCommand` is exact argv (never a prefix); and
/// `allowManagedMcpServersOnly` requires a positive grant (fail closed).
/// Rows are (server, verdict, why).
#[test]
#[rustfmt::skip]
fn name_argv_and_lockdown_semantics() {
    check_allowed(
        "command-only allowlist",
        &allowlist_from(
            serde_json::json!({
            "allowedMcpServers": [ { "command": "npx" } ]
        }),
        ),
        &[
            (h("https://any.example/mcp"), true, "never restricts HTTP"),
            (ss("ok", "npx"), true, "listed command runs"),
            (ss("no", "other"), false, "unlisted command blocked"),
        ],
    );
    check_allowed(
        "URL-only allowlist",
        &allow_urls(&["https://ok.example/*"]),
        &[
            (ss("s", "anything"), true, "never restricts stdio"),
            (hs("ok", "https://ok.example/mcp"), true, "listed URL runs"),
            (h("https://other.example/x"), false, "unlisted URL blocked"),
        ],
    );
    let deny_only = allowlist_from(
        serde_json::json!({
        "deniedMcpServers": [
            { "serverUrl": "https://mcp-gateway.example.net/*" },
            { "command": "npx" }
        ]
    }),
    );
    assert!(
        deny_only.is_restricted(),
        "deny-only must still count as restricted so enforcement engages"
    );
    check_allowed(
        "deny-only policy",
        &deny_only,
        &[
            (h("https://mcp-gateway.example.net/mcp"), false, "denied URL blocked"),
            (
                h("https://other.com/mcp"),
                true,
                "empty allowlist still allows the un-denied rest",
            ),
            (ss("n", "npx"), false, "denied command blocked"),
            (ss("n", "node"), true, "other commands run"),
            (
                ss("n", "/usr/local/bin/npx"),
                true,
                "command deny is exact-string, never a suffix",
            ),
        ],
    );
    check_allowed(
        "URL-only denylist",
        &deny_urls(&["https://blocked.com/*"]),
        &[
            (ss("s", "anything"), true, "never restricts stdio"),
            (h("https://blocked.com/mcp"), false, "while still denying its URL"),
        ],
    );
    let deny_beats_allow = allowlist_from(
        serde_json::json!({
        "allowedMcpServers": [
            { "serverUrl": "https://*.example.com/*" },
            { "command": "npx" }
        ],
        "deniedMcpServers": [
            { "serverUrl": "https://blocked.example.com/*" },
            { "command": "npx" }
        ]
    }),
    );
    check_allowed(
        "deny beats allow (URL and command)",
        &deny_beats_allow,
        &[
            (h("https://ok.example.com/mcp"), true, "allowlisted URL runs"),
            (
                h("https://blocked.example.com/mcp"),
                false,
                "deny wins over the URL allow",
            ),
            (ss("n", "npx"), false, "deny wins over the command allow"),
        ],
    );
    check_denied(
        "deny beats allow (URL and command)",
        &deny_beats_allow,
        &[
            (
                h("https://blocked.example.com/mcp"),
                true,
                "URL deny classifies as denied",
            ),
            (ss("n", "npx"), true, "command deny classifies as denied"),
        ],
    );
    let deny_name = allowlist_from(
        serde_json::json!({
        "deniedMcpServers": [ { "serverName": "foo" } ]
    }),
    );
    assert!(deny_name.is_restricted(), "a name denylist restricts");
    check_denied(
        "deny serverName foo",
        &deny_name,
        &[
            (hs("foo", "https://x.example/mcp"), true, "bare runtime name"),
            (
                hs("grok_com_foo", "https://x.example/mcp"),
                true,
                "managed-prefixed runtime name",
            ),
            (ss("grok_com_foo", "npx"), true, "name match is transport-agnostic"),
            (
                hs("foobar", "https://x.example/mcp"),
                false,
                "exact after strip, never substring",
            ),
            (
                hs("grok_com_foobar", "https://x.example/mcp"),
                false,
                "prefixed near-miss",
            ),
            (hs("barfoo", "https://x.example/mcp"), false, "suffix near-miss"),
            (hs("bar", "https://x.example/mcp"), false, "unrelated name"),
        ],
    );
    check_allowed(
        "deny serverName foo",
        &deny_name,
        &[
            (hs("foo", "https://x.example/mcp"), false, "denied bare name"),
            (hs("grok_com_foo", "https://x.example/mcp"), false, "denied managed name"),
            (ss("grok_com_foo", "npx"), false, "denied on any transport"),
            (
                hs("foobar", "https://x.example/mcp"),
                true,
                "unrelated names remain allowed",
            ),
        ],
    );
    let allow_name = allowlist_from(
        serde_json::json!({
        "allowedMcpServers": [ { "serverName": "foo" } ]
    }),
    );
    assert!(allow_name.is_restricted(), "a name allowlist restricts");
    check_allowed(
        "allow serverName foo",
        &allow_name,
        &[
            (
                hs("foo", "https://anything.example/x"),
                true,
                "named server allowed on any URL",
            ),
            (
                hs("grok_com_foo", "https://evil.example/x"),
                true,
                "managed spelling, any URL",
            ),
            (ss("grok_com_foo", "/usr/bin/whatever"), true, "allowed on any transport"),
            (hs("bar", "https://anything.example/x"), false, "unlisted name blocked"),
            (ss("bar", "npx"), false, "unlisted stdio blocked too"),
        ],
    );
    check_denied(
        "allow serverName foo",
        &allow_name,
        &[
            (
                hs("bar", "https://anything.example/x"),
                false,
                "blocked as missing, not denied",
            ),
        ],
    );
    let name_both = allowlist_from(
        serde_json::json!({
        "allowedMcpServers": [ { "serverName": "foo" } ],
        "deniedMcpServers":  [ { "serverName": "foo" } ]
    }),
    );
    check_denied(
        "serverName deny beats allow",
        &name_both,
        &[
            (hs("foo", "https://foo.example/x"), true, "bare spelling"),
            (hs("grok_com_foo", "https://foo.example/x"), true, "managed spelling"),
        ],
    );
    check_allowed(
        "serverName deny beats allow",
        &name_both,
        &[
            (hs("foo", "https://foo.example/x"), false, "deny wins for the same name"),
            (
                hs("grok_com_foo", "https://foo.example/x"),
                false,
                "deny wins, managed spelling",
            ),
        ],
    );
    check_denied(
        "prefixed policy entry vs bare runtime (vice versa)",
        &allowlist_from(
            serde_json::json!({
            "deniedMcpServers": [ { "serverName": "grok_com_foo" } ]
        }),
        ),
        &[
            (
                hs("foo", "https://x.example/mcp"),
                true,
                "bare runtime matches after strip",
            ),
            (
                hs("grok_com_foo", "https://x.example/mcp"),
                true,
                "prefixed runtime matches",
            ),
            (hs("foobar", "https://x.example/mcp"), false, "near-miss unrelated"),
            (hs("grok_com_foobar", "https://x.example/mcp"), false, "prefixed near-miss"),
        ],
    );
    check_allowed(
        "allow: URL ∪ name — either dimension grants",
        &allowlist_from(
            serde_json::json!({
            "allowedMcpServers": [
                { "serverUrl": "https://ok.example.com/*" },
                { "serverName": "foo" }
            ]
        }),
        ),
        &[
            (hs("bar", "https://ok.example.com/mcp"), true, "URL dimension grants"),
            (hs("foo", "https://evil.example.com/mcp"), true, "name dimension grants"),
            (
                hs("bar", "https://evil.example.com/mcp"),
                false,
                "neither dimension: blocked",
            ),
        ],
    );
    let deny_dims = allowlist_from(
        serde_json::json!({
        "deniedMcpServers": [ { "command": "npx" }, { "serverName": "foo" } ]
    }),
    );
    check_denied(
        "deny: command and name deny independently",
        &deny_dims,
        &[
            (ss("unrelated", "npx"), true, "command deny hits regardless of name"),
            (ss("foo", "node"), true, "name deny hits regardless of command"),
            (ss("unrelated", "node"), false, "neither deny dimension"),
        ],
    );
    check_allowed(
        "deny: command and name deny independently",
        &deny_dims,
        &[(ss("unrelated", "node"), true, "undenied server stays allowed")],
    );
    let argv_policy = allowlist_from(
        serde_json::json!({
        "allowedMcpServers": [
            { "serverCommand": ["npx", "@example-corp/ui-kit-mcp", "enterprise-webc"] }
        ],
        "deniedMcpServers": [
            { "serverCommand": ["npx", "evil-mcp"] }
        ]
    }),
    );
    check_allowed(
        "serverCommand arrays: exact argv",
        &argv_policy,
        &[
            (
                sa("k", "npx", &["@example-corp/ui-kit-mcp", "enterprise-webc"]),
                true,
                "exact argv",
            ),
            (
                sa("k", "npx", &["@example-corp/ui-kit-mcp", "client-react"]),
                false,
                "other variant",
            ),
            (
                sa(
                    "k",
                    "npx",
                    &["@example-corp/ui-kit-mcp", "enterprise-webc", "--evil"],
                ),
                false,
                "prefix + extra trailing arg is not granted",
            ),
            (sa("evil", "npx", &["evil-mcp"]), false, "denied argv is blocked"),
        ],
    );
    check_denied(
        "serverCommand arrays: exact argv",
        &argv_policy,
        &[
            (sa("evil", "npx", &["evil-mcp"]), true, "deny argv classifies as denied"),
            (sa("bare", "npx", &[]), false, "a shorter argv is not denied"),
        ],
    );
    let lockdown = allowlist_from(
        serde_json::json!({
        "allowManagedMcpServersOnly": true,
        "allowedMcpServers": [
            { "serverUrl": "https://mcp.figma.com/*" },
            { "serverCommand": ["uvx", "mcp-grafana"] }
        ]
    }),
    );
    assert!(lockdown.managed_only(FOREIGN));
    assert!(lockdown.is_restricted());
    check_allowed(
        "managed-only lockdown requires a positive grant",
        &lockdown,
        &[
            (hs("figma", "https://mcp.figma.com/mcp"), true, "allowlisted URL runs"),
            (sa("grafana", "uvx", &["mcp-grafana"]), true, "allowlisted argv runs"),
            (ss("rogue", "python3"), false, "stdio can't ride a URL-only allowlist"),
            (h("https://evil.example.com/mcp"), false, "unlisted URL fails closed"),
        ],
    );
    let empty_lockdown = allowlist_from(
        serde_json::json!({ "allowManagedMcpServersOnly": true }),
    );
    assert!(empty_lockdown.is_restricted());
    check_allowed(
        "managed-only with an empty allowlist blocks everything",
        &empty_lockdown,
        &[
            (h("https://any.example.com/mcp"), false, "any HTTP blocked"),
            (ss("any", "npx"), false, "any stdio blocked"),
        ],
    );
}
/// What this table pins: policy `serverName` ↔ runtime-name identity — both
/// sides strip `grok_com_` and normalize (lowercase, spaces → `_`), compare
/// by exact equality (never substring, empty never matches), and legacy
/// truncation applies only to the one shape the runtime actually truncates
/// (a managed name at exactly the cap), so a long entry never becomes a
/// prefix grant over attacker-chosen decoys. Rows are (pattern, name,
/// matches?, why).
#[test]
#[rustfmt::skip]
fn mcp_name_matching_semantics() {
    let max_bare = MANAGED_MCP_NAME_MAX_CHARS - MANAGED_MCP_PREFIX.len();
    let long = "a".repeat(MANAGED_MCP_NAME_MAX_CHARS * 2);
    let long_runtime = format!("{MANAGED_MCP_PREFIX}{}", &long[..max_bare]);
    let long_bare = "a".repeat(max_bare + 8);
    let entry = format!("{MANAGED_MCP_PREFIX}{long_bare}");
    let plain_decoy = format!("{}-decoy", &long_bare[..max_bare]);
    let at_cap = format!("{MANAGED_MCP_PREFIX}{}", &long_bare[..max_bare]);
    let over_cap_decoy = format!("{MANAGED_MCP_PREFIX}{}-decoy", &long_bare[..max_bare]);
    let under_cap_decoy = format!("{MANAGED_MCP_PREFIX}{}", &long_bare[..max_bare - 4]);
    let corp = "corporate-approved-server-alpha-prod";
    let rows: Vec<(&str, &str, bool, &str)> = vec![
        ("foo", "foo", true, "exact bare match"),
        ("foo", "grok_com_foo", true, "bare entry vs managed runtime"),
        ("grok_com_foo", "foo", true, "managed entry vs bare runtime"),
        ("grok_com_foo", "grok_com_foo", true, "both managed"),
        ("foo", "foobar", false, "never substring"),
        ("foo", "grok_com_foobar", false, "never substring, managed"),
        ("foo", "barfoo", false, "never suffix"),
        ("foo", "bar", false, "different name"),
        ("", "foo", false, "empty entry never matches"),
        ("Slack", "grok_com_slack", true, "display case folds"),
        ("My Server", "grok_com_my_server", true, "spaces normalize to underscores"),
        ("grok_com_my_server", "My Server", true, "managed entry vs display runtime"),
        ("My Server", "my_server", true, "display entry vs local runtime"),
        ("SLACK", "slack", true, "all-caps entry"),
        ("My Server", "my_server_2", false, "normalized near-miss"),
        ("", "", false, "both empty never match"),
        ("grok_com_", "grok_com_anything", false, "bare prefix key is empty"),
        (&long, &long_runtime, true, "too-long entry matches its truncated runtime name"),
        (corp, "corporate-approved-server-alpha-anything", false, "long plain names don't collide"),
        (corp, corp, true, "exact long plain names still match"),
        (&entry, &plain_decoy, false, "long managed entry is no prefix grant over a plain decoy"),
        (&entry, &at_cap, true, "matches its own truncated managed runtime name"),
        (&entry, &over_cap_decoy, false, "over-cap managed decoy does not prefix-match"),
        (&entry, &under_cap_decoy, false, "under-cap managed decoy does not prefix-match"),
    ];
    for (pattern, name, want, label) in rows {
        assert_eq!(mcp_name_matches(pattern, name), want, "{label}");
    }
    for (input, want, label) in [
        ("Slack", "slack", "lowercases"),
        ("My Server", "my_server", "spaces become underscores"),
        ("My  Server", "my__server", "double space keeps both underscores"),
        ("", "", "empty is identity"),
    ] {
        assert_eq!(normalize_managed_name(input), want, "{label}");
    }
}
/// Assert one `mcp_verdict` outcome; `want` is the pinned `Display` string
/// of the block reason (`None` = allowed).
fn check_verdict(
    ms: &ManagedSettings,
    server: &agent_client_protocol::McpServer,
    subject: McpSubject,
    want: Option<&str>,
    label: &str,
) {
    match (ms.mcp_verdict(server, subject), want) {
        (McpVerdict::Allowed, None) => {}
        (McpVerdict::Blocked(reason), Some(want)) => {
            assert_eq!(reason.to_string(), want, "{label}");
        }
        (McpVerdict::Allowed, Some(want)) => {
            panic!("{label}: allowed, wanted block {want:?}")
        }
        (McpVerdict::Blocked(reason), None) => {
            panic!("{label}: blocked with {reason}, wanted allowed")
        }
    }
}
/// What this matrix pins: `mcp_verdict` reason precedence — a deny wins over
/// the lockdown's missing grant, which wins over the project pin; an allow
/// grant exempts the pin — with each reason's `Display` string pinned as the
/// wire/UX payload it is.
#[test]
fn mcp_verdict_matrix() {
    let foreign = McpSubject {
        origin: FOREIGN,
        project_scoped: false,
    };
    let project = McpSubject {
        origin: FOREIGN,
        project_scoped: true,
    };
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "deniedMcpServers": [ { "serverName": "blocked" } ],
            "enableAllProjectMcpServers": false
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let deny_msg = format!("matches deniedMcpServers ({CLAUDE_PATH})");
    let pin_msg =
        format!("project MCP disabled (enableAllProjectMcpServers = false, {CLAUDE_PATH})");
    let url = "https://mcp.corp.com/mcp";
    check_verdict(&ms, &hs("blocked", url), foreign, Some(&deny_msg), "deny");
    check_verdict(
        &ms,
        &hs("blocked", url),
        project,
        Some(&deny_msg),
        "deny attributed before the project pin",
    );
    check_verdict(
        &ms,
        &hs("ok", url),
        foreign,
        None,
        "non-denied non-project runs",
    );
    check_verdict(
        &ms,
        &hs("ok", url),
        project,
        Some(&pin_msg),
        "project pin blocks an ungranted project server",
    );
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "allowedMcpServers": [ { "serverUrl": "https://mcp.corp.com/*" } ],
            "deniedMcpServers": [ { "serverName": "blocked" } ],
            "allowManagedMcpServersOnly": true
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let not_granted_msg = format!("not in allowedMcpServers ({CLAUDE_PATH})");
    check_verdict(
        &ms,
        &h(url),
        foreign,
        None,
        "granted URL runs under lockdown",
    );
    check_verdict(
        &ms,
        &h("https://other.example/mcp"),
        foreign,
        Some(&not_granted_msg),
        "ungranted URL is not-granted under lockdown",
    );
    check_verdict(
        &ms,
        &hs("blocked", "https://other.example/mcp"),
        foreign,
        Some(&deny_msg),
        "deny wins over not-granted",
    );
    let ms = parse_managed_settings_json(
        &serde_json::json!({
            "allowedMcpServers": [ { "serverUrl": "https://mcp.corp.com/*" } ],
            "enableAllProjectMcpServers": false
        }),
        std::path::Path::new(CLAUDE_PATH),
    );
    let ungranted = ss("proj", "python3");
    assert_eq!(
        ms.mcp_project_pin_block(&ungranted, project)
            .map(|r| r.to_string()),
        Some(pin_msg),
        "pin blocks an ungranted project server"
    );
    assert_eq!(
        ms.mcp_project_pin_block(&h(url), project)
            .map(|r| r.to_string()),
        None,
        "an allow-entry grant exempts the pin"
    );
    assert_eq!(
        ms.mcp_project_pin_block(&ungranted, foreign)
            .map(|r| r.to_string()),
        None,
        "non-project subjects are never pinned"
    );
    assert_eq!(
        ManagedSettings::default()
            .mcp_project_pin_block(&ungranted, project)
            .map(|r| r.to_string()),
        None,
        "no active pin, no block"
    );
}
/// `McpBlockReason::Display` strings are wire/UX payloads (pager rows,
/// doctor details, enable errors) — pin them and `source()` directly.
#[test]
fn mcp_block_reason_display_and_source_are_pinned() {
    let src = PathBuf::from("/etc/grok/requirements.toml");
    let cases = [
        (
            McpBlockReason::Deny {
                source: src.clone(),
            },
            "matches deniedMcpServers (/etc/grok/requirements.toml)",
        ),
        (
            McpBlockReason::NotGranted {
                source: src.clone(),
            },
            "not in allowedMcpServers (/etc/grok/requirements.toml)",
        ),
        (
            McpBlockReason::ProjectPin {
                source: src.clone(),
            },
            "project MCP disabled (enableAllProjectMcpServers = false, /etc/grok/requirements.toml)",
        ),
    ];
    for (reason, want) in cases {
        assert_eq!(reason.to_string(), want);
        assert_eq!(reason.source(), src.as_path());
    }
    let ms = ManagedSettings {
        mcp_allowlist: McpServerPolicy::single(
            McpServerAllowlist::new(vec![], vec![], None).with_managed_only(),
        ),
        ..Default::default()
    };
    let subject = McpSubject {
        origin: FOREIGN,
        project_scoped: false,
    };
    match ms.mcp_verdict(&h("https://any.example/mcp"), subject) {
        McpVerdict::Blocked(reason @ McpBlockReason::NotGranted { .. }) => {
            assert_eq!(reason.to_string(), "not in allowedMcpServers ()");
            assert_eq!(reason.source(), std::path::Path::new(""));
        }
        other => panic!("wanted the empty-source NotGranted fallback, got {other:?}"),
    }
}
/// Test sink that accumulates `tracing` output into a shared buffer.
#[derive(Clone)]
struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Serializes capture tests: `rebuild_interest_cache` is process-global, so
/// two concurrent captures can drop each other's warns.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Run `f` while capturing WARN-level logs on this thread.
fn capturing_warn_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    {
        static GLOBAL_SINK: std::sync::Once = std::sync::Once::new();
        GLOBAL_SINK.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::WARN)
                    .with_writer(std::io::sink)
                    .finish(),
            );
        });
    }
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer_buf = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || VecWriter(writer_buf.clone()))
        .finish();
    let value = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    (value, logs)
}
/// Parse `key` while capturing WARN-level logs on this thread.
fn parse_mcp_entries_capturing_logs(
    json: &serde_json::Value,
    list: McpPolicyList,
) -> (Vec<AllowedMcpServer>, String) {
    capturing_warn_logs(|| parse_mcp_entry_list(json, list))
}
/// What this table pins: parse-time fail directions. Unenforceable DENY
/// entries and unmatchable shapes on either list warn (a silent drop is zero
/// enforcement on the deny side and silently lost grants on the allow side),
/// while merely-unsupported ALLOW entries stay silent (ungranted =
/// fail-closed).
#[test]
fn mcp_entry_parse_fail_directions() {
    use McpPolicyList::{Allow, Deny};
    struct ParseCase {
        label: &'static str,
        list: McpPolicyList,
        entries: serde_json::Value,
        /// Entries surviving the parse.
        want_len: usize,
        /// Exact occurrence counts of a log line.
        counts: &'static [(&'static str, usize)],
        /// Log lines that must appear at least once.
        contains: &'static [&'static str],
        /// Log lines that must not appear.
        absent: &'static [&'static str],
    }
    impl Default for ParseCase {
        fn default() -> Self {
            Self {
                label: "",
                list: Deny,
                entries: serde_json::Value::Null,
                want_len: 0,
                counts: &[],
                contains: &[],
                absent: &[],
            }
        }
    }
    let cases = vec![
        ParseCase {
            label: "allow: glob-scheme and scheme-less patterns fail closed but warn",
            list: Allow,
            entries: serde_json::json!([
                { "serverUrl": "*://mcp.corp.com/*" },
                { "serverUrl": "*.corp.com/*" },
                { "serverUrl": "https://mcp.corp.com/*" }
            ]),
            want_len: 3,
            counts: &[("can never match", 2)],
            ..Default::default()
        },
        ParseCase {
            label: "allow: dead IP/label/port shapes warn; working shapes stay silent",
            list: Allow,
            entries: serde_json::json!([
                { "serverUrl": "http://127.1/*" },
                { "serverUrl": "https://2001:db8::1/*" },
                { "serverUrl": "https://bü*.example/*" },
                // Working shapes the warn must NOT fire on: bracketed and
                // canonical IPs, canonical-address-plus-port, trailing dot.
                { "serverUrl": "https://[2001:0db8::1]/*" },
                { "serverUrl": "https://127.0.0.1/*" },
                { "serverUrl": "https://2001:db8::1:443/*" },
                { "serverUrl": "https://127.0.0.1./*" },
                { "serverUrl": "https://2001:db8*:443/*" },
                // Dead shapes: no host, glob port — bracketed IPv6 included.
                { "serverUrl": "https:///admin/*" },
                { "serverUrl": "https://mcp.corp.com:*/mcp/*" },
                { "serverUrl": "https://[::1]:*/*" },
                { "serverUrl": "https://[::1]:http/*" },
                // Working shape: bracketed address with a numeric port.
                { "serverUrl": "https://[::1]:8080/*" }
            ]),
            want_len: 13,
            counts: &[("can never match", 7)],
            contains: &["has no host", "port is not a number"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: host-less and non-compiling host globs warn (zero enforcement)",
            list: Deny,
            entries: serde_json::json!([
                { "serverUrl": "/admin/*" },
                { "serverUrl": "https://host[x/*" },
                { "serverUrl": "https://blocked.example.com/*" }
            ]),
            want_len: 3,
            contains: &["has no host", "host glob does not compile"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: unsupported entry warns and is dropped",
            list: Deny,
            entries: serde_json::json!([
                { "serverTypo": "internal-only" },
                { "serverUrl": "https://blocked.com/*" }
            ]),
            want_len: 1,
            contains: &["ignoring unsupported deniedMcpServers entry"],
            ..Default::default()
        },
        ParseCase {
            label: "allow: unsupported entry stays silent (ungranted = fail closed)",
            list: Allow,
            entries: serde_json::json!([ { "serverTypo": "internal-only" } ]),
            want_len: 0,
            absent: &["ignoring unsupported"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: serverName is first-class — parsed, no warn",
            list: Deny,
            entries: serde_json::json!([ { "serverName": "internal-only" } ]),
            want_len: 1,
            absent: &["ignoring unsupported"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: serverCommand array is enforced, not warn-dropped",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": ["npx", "evil-mcp"] } ]),
            want_len: 1,
            absent: &["ignoring unsupported"],
            ..Default::default()
        },
        ParseCase {
            label: "deny: malformed serverCommand argv warn-drops (a partial argv \
                    would match the wrong command)",
            list: Deny,
            entries: serde_json::json!([ { "serverCommand": ["npx", 42] } ]),
            want_len: 0,
            contains: &["ignoring unsupported"],
            ..Default::default()
        },
    ];
    for case in cases {
        let key = match case.list {
            Allow => "allowedMcpServers",
            Deny => "deniedMcpServers",
        };
        let label = case.label;
        let json = serde_json::json!({ key: case.entries });
        let (entries, logs) = parse_mcp_entries_capturing_logs(&json, case.list);
        assert_eq!(entries.len(), case.want_len, "{label}: surviving entries");
        for (needle, count) in case.counts {
            assert_eq!(
                logs.matches(needle).count(),
                *count,
                "{label}: count of {needle:?}, got: {logs:?}"
            );
        }
        for needle in case.contains {
            assert!(
                logs.contains(needle),
                "{label}: missing {needle:?} in {logs:?}"
            );
        }
        for needle in case.absent {
            assert!(
                !logs.contains(needle),
                "{label}: unexpected {needle:?} in {logs:?}"
            );
        }
    }
    let (entries, _) = parse_mcp_entries_capturing_logs(
        &serde_json::json!({ "deniedMcpServers": [ { "serverName": "internal-only" } ] }),
        Deny,
    );
    assert!(
        matches!(&entries[0], AllowedMcpServer::Name { name } if name == "internal-only"),
        "expected a Name entry, got {entries:?}"
    );
}
/// What this table pins: the canonical git-URL identity — scheme and host
/// fold case, repo paths stay case-sensitive (lowercasing would widen an
/// allow entry to sibling repos on a case-sensitive host), exactly one
/// `.git` strips, local paths pass through, and the scp and https forms of
/// one repo never alias each other — plus the marketplace allowlist matching
/// built on it. Rows are (input(s), expected, why).
#[test]
#[rustfmt::skip]
fn git_url_identity_and_marketplace_matching() {
    let rows: [(&str, &str, &str); 4] = [
        (
            "HTTPS://Git.Corp.com/Team/Tools.git",
            "https://git.corp.com/Team/Tools",
            "scheme+host fold case; one .git strips",
        ),
        (
            "https://h.example/repo.git.git",
            "https://h.example/repo.git",
            "`repo.git.git` is a repo named `repo.git`",
        ),
        (
            "Git@GitHub.com:Org/Repo.git",
            "git@github.com:Org/Repo",
            "scp-style: user@host folds, path does not",
        ),
        ("/tmp/Marketplace", "/tmp/Marketplace", "local paths pass through untouched"),
    ];
    for (input, want, label) in rows {
        assert_eq!(normalize_git_url(input), want, "{label}");
    }
    assert_ne!(
        normalize_git_url("https://git.corp.com/team/tools"),
        normalize_git_url("https://git.corp.com/Team/Tools"),
        "path case is identity: a different-cased path is a different repo"
    );
    let ent = "git@github.enterprise.example:ACME/repo.git";
    let match_rows: [(&str, &str, bool, &str); 7] = [
        (ent, "git@github.enterprise.example:ACME/repo.git", true, "exact spelling"),
        (
            ent,
            "git@github.enterprise.example:ACME/repo",
            true,
            ".git suffix is optional",
        ),
        (ent, "git@GITHUB.ENTERPRISE.EXAMPLE:ACME/repo.git", true, "host case folds"),
        (
            ent,
            "git@github.enterprise.example:acme/repo.git",
            false,
            "path case earns no grant",
        ),
        (ent, "git@evil.com:ACME/repo.git", false, "different host"),
        (
            "git@github.com:Org/Repo.git",
            "https://github.com/Org/Repo.git",
            false,
            "scp-form entry does not alias the https form of the same repo",
        ),
        (
            "https://github.com/Org/Repo.git",
            "git@github.com:Org/Repo.git",
            false,
            "https-form entry does not alias the scp form",
        ),
    ];
    for (entry, url, want, label) in match_rows {
        let al = MarketplaceAllowlist {
            allowed_urls: vec![entry.into()],
            source_path: None,
            authority: PolicySourceAuthority::Native,
        };
        assert_eq!(al.is_url_allowed(url), want, "{label}");
    }
}
/// [`resolve_managed_settings`] over inline sources: an optional Claude JSON
/// plus `(tier, path, toml)` layers.
fn layered(
    claude: Option<serde_json::Value>,
    layers: &[(PolicyLayerTier, &str, &str)],
) -> ManagedSettings {
    resolve_managed_settings(
        claude.map(|json| (json, PathBuf::from(CLAUDE_PATH))),
        layers
            .iter()
            .map(|(tier, path, toml_str)| PolicyLayer {
                tier: *tier,
                path: PathBuf::from(path),
                value: toml::from_str(toml_str).unwrap(),
            })
            .collect(),
    )
}
/// One layer-resolution expectation; each variant names the surface it pins.
enum Expect {
    /// `is_server_allowed` for an HTTP server (name, url, origin, want).
    Allowed(&'static str, &'static str, PolicySubjectOrigin, bool),
    /// `is_server_denied` for an HTTP server (name, url, origin, want).
    Denied(&'static str, &'static str, PolicySubjectOrigin, bool),
    /// `is_server_denied` for a stdio server with exactly this argv.
    ArgvDenied(&'static [&'static str], bool),
    ManagedOnly(PolicySubjectOrigin, bool),
    /// Project-MCP pin state: `Some(path)` = disabled, attributed there.
    ProjectMcpPin(Option<&'static str>),
    /// Plugin auto-update pin state: `Some(path)` = disabled, attributed there.
    AutoUpdatePin(Option<&'static str>),
    MarketRestricted(bool),
    MarketUrl(&'static str, PolicySubjectOrigin, bool),
    /// `add_block_reason(url).is_some()`.
    MarketAddBlocked(&'static str, bool),
    ExtrasCount(usize),
    /// `extra_marketplaces[idx]` is a Git marketplace (name, url, ref).
    ExtraGit(usize, &'static str, &'static str, Option<&'static str>),
}
fn assert_expects(label: &str, ms: &ManagedSettings, expects: Vec<Expect>) {
    for e in expects {
        match e {
            Expect::Allowed(name, url, origin, want) => {
                assert_eq!(
                    ms.mcp_allowlist.is_server_allowed(&hs(name, url), origin),
                    want,
                    "{label}: allowed({url}, {origin:?})"
                )
            }
            Expect::Denied(name, url, origin, want) => {
                assert_eq!(
                    ms.mcp_allowlist.is_server_denied(&hs(name, url), origin),
                    want,
                    "{label}: denied({url}, {origin:?})"
                )
            }
            Expect::ArgvDenied(argv, want) => {
                assert_eq!(
                    ms.mcp_allowlist
                        .is_server_denied(&sa("t", argv[0], &argv[1..]), FOREIGN),
                    want,
                    "{label}: argv denied({argv:?})"
                )
            }
            Expect::ManagedOnly(origin, want) => {
                assert_eq!(
                    ms.mcp_allowlist.managed_only(origin),
                    want,
                    "{label}: managed_only({origin:?})"
                )
            }
            Expect::ProjectMcpPin(source) => {
                assert_eq!(
                    ms.project_mcp.source(),
                    source.map(Path::new),
                    "{label}: project_mcp pin"
                )
            }
            Expect::AutoUpdatePin(source) => {
                assert_eq!(
                    ms.plugin_auto_update.source(),
                    source.map(Path::new),
                    "{label}: auto-update pin"
                )
            }
            Expect::MarketRestricted(want) => {
                assert_eq!(
                    ms.marketplace_allowlist.is_restricted(),
                    want,
                    "{label}: marketplace restricted"
                )
            }
            Expect::MarketUrl(url, origin, want) => {
                assert_eq!(
                    ms.marketplace_allowlist.is_url_allowed(url, origin),
                    want,
                    "{label}: marketplace url({url}, {origin:?})"
                )
            }
            Expect::MarketAddBlocked(url, want) => {
                assert_eq!(
                    ms.marketplace_allowlist.add_block_reason(url).is_some(),
                    want,
                    "{label}: add gate({url})"
                )
            }
            Expect::ExtrasCount(n) => {
                assert_eq!(ms.extra_marketplaces.len(), n, "{label}: extras count")
            }
            Expect::ExtraGit(idx, name, url, git_ref) => {
                let extra = &ms.extra_marketplaces[idx];
                assert_eq!(extra.name, name, "{label}: extra[{idx}] name");
                assert_eq!(
                    extra.kind,
                    ManagedMarketplaceKind::Git {
                        url: url.into(),
                        git_ref: git_ref.map(String::from),
                    },
                    "{label}: extra[{idx}] kind"
                );
            }
        }
    }
}
/// What these scenarios pin: layer resolution is strictest-wins (any deny
/// wins, every restricted source must allow — intersection, never union),
/// pins only tighten and are attributed to the first pinning layer in trust
/// order, extras names resolve trust-descending regardless of load order,
/// grok's own TOML layers bind native subjects while the vendor Claude file
/// is advisory (foreign subjects only), and malformed values degrade
/// per-key, never dropping a layer's healthy pins.
#[test]
fn layer_resolution_semantics() {
    assert_expects(
        "requirements TOML pins combine strictest-wins with the Claude source",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [
                    { "serverUrl": "https://ok.example.com/*" },
                    { "serverUrl": "https://user-extra.example.com/*" }
                ]
            })),
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
allow_managed_mcp_servers_only = true
enable_all_project_mcp_servers = false
plugin_auto_update = false

[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"

[[denied_mcp_servers]]
server_command = ["npx", "evil-mcp"]

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/example-corp/approved-plugins.git"

[[strict_known_marketplaces]]
source = "github"
repo = "acme/approved-plugins"
ref = "stable"

[extra_known_marketplaces.approved-plugins]
source = { source = "git", url = "https://github.com/example-corp/approved-plugins.git", ref = "main" }
"#,
            )],
        ),
        vec![
            Expect::Allowed("ok", "https://ok.example.com/mcp", FOREIGN, true),
            // Allowed by the Claude source alone — blocked by the pin.
            Expect::Allowed(
                "extra",
                "https://user-extra.example.com/mcp",
                FOREIGN,
                false,
            ),
            Expect::ArgvDenied(&["npx", "evil-mcp"], true),
            Expect::ManagedOnly(FOREIGN, true),
            Expect::ProjectMcpPin(Some(SYS_REQ)),
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::MarketRestricted(true),
            Expect::MarketUrl(
                "https://github.com/example-corp/approved-plugins.git",
                FOREIGN,
                true,
            ),
            // github+repo strict entry participates (ref tolerated).
            Expect::MarketUrl(
                "https://github.com/acme/approved-plugins.git",
                FOREIGN,
                true,
            ),
            Expect::MarketUrl("https://github.com/evil/repo.git", FOREIGN, false),
            Expect::ExtrasCount(1),
            Expect::ExtraGit(
                0,
                "approved-plugins",
                "https://github.com/example-corp/approved-plugins.git",
                Some("main"),
            ),
        ],
    );
    assert_expects(
        "first-wins resolution is trust-descending, not load-order",
        &layered(
            Some(serde_json::json!({
                "pluginAutoUpdate": false,
                "extraKnownMarketplaces": {
                    "corp": { "source": { "source": "git", "url": "https://github.com/claude/evil.git" } }
                }
            })),
            &[
                (
                    PolicyLayerTier::UserRequirements,
                    USER_REQ,
                    r#"
plugin_auto_update = false

[extra_known_marketplaces.corp]
source = { source = "git", url = "https://github.com/user/evil.git" }
"#,
                ),
                (
                    PolicyLayerTier::SystemRequirements,
                    SYS_REQ,
                    r#"
plugin_auto_update = false

[extra_known_marketplaces.corp]
source = { source = "git", url = "https://github.com/corp/approved.git", ref = "stable" }
"#,
                ),
            ],
        ),
        vec![
            Expect::ExtrasCount(1),
            // The admin layer's URL claims the name; squats are skipped.
            Expect::ExtraGit(
                0,
                "corp",
                "https://github.com/corp/approved.git",
                Some("stable"),
            ),
            // Pin attribution names the admin layer, not the user file.
            Expect::AutoUpdatePin(Some(SYS_REQ)),
        ],
    );
    assert_expects(
        "non-finite float elsewhere in a layer keeps its policy pins",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
unrelated_tuning = inf
plugin_auto_update = false

[[denied_mcp_servers]]
server_name = "blocked"
"#,
            )],
        ),
        vec![
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::Denied("blocked", "https://x.example.com/mcp", FOREIGN, true),
        ],
    );
    assert_expects(
        "wrong-typed policy lists do not drop sibling keys",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
plugin_auto_update = false
denied_mcp_servers = { server_url = "https://evil.example.com/*" }
strict_known_marketplaces = "https://github.com/corp/x.git"
"#,
            )],
        ),
        vec![
            Expect::Denied("evil", "https://evil.example.com/mcp", FOREIGN, false),
            Expect::MarketRestricted(false),
            Expect::AutoUpdatePin(Some(SYS_REQ)),
        ],
    );
    assert_expects(
        "unstringifiable values inside deny entries warn-drop those entries only",
        &layered(
            None,
            &[
                (
                    PolicyLayerTier::SystemRequirements,
                    SYS_REQ,
                    r#"
plugin_auto_update = false

[[denied_mcp_servers]]
server_url = 1979-05-27T07:32:00Z
"#,
                ),
                (
                    PolicyLayerTier::UserRequirements,
                    USER_REQ,
                    r#"
enable_all_project_mcp_servers = false

[[denied_mcp_servers]]
server_url = inf
"#,
                ),
            ],
        ),
        vec![
            Expect::AutoUpdatePin(Some(SYS_REQ)),
            Expect::ProjectMcpPin(Some(USER_REQ)),
            Expect::Denied("t", "https://x.example.com/mcp", FOREIGN, false),
        ],
    );
    assert_expects(
        "the vendor Claude JSON is advisory: binds foreign subjects only",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [ { "serverUrl": "https://ok.example.com/*" } ],
                "deniedMcpServers": [ { "serverUrl": "https://denied.example.com/*" } ],
                "allowManagedMcpServersOnly": true,
                "strictKnownMarketplaces": [
                    { "source": "git", "url": "https://github.com/example-corp/approved-plugins.git" }
                ]
            })),
            &[],
        ),
        vec![
            // Foreign subjects (project files, imported editor configs,
            // client injection): the vendor file binds.
            Expect::Allowed("denied", "https://denied.example.com/mcp", FOREIGN, false),
            Expect::Allowed(
                "unlisted",
                "https://unlisted.example.com/mcp",
                FOREIGN,
                false,
            ),
            // grok-native subjects (user/system config.toml, plugins):
            // advisory — the same servers still run.
            Expect::Allowed("denied", "https://denied.example.com/mcp", NATIVE, true),
            Expect::Allowed("unlisted", "https://unlisted.example.com/mcp", NATIVE, true),
            Expect::MarketUrl("https://github.com/other/repo.git", FOREIGN, false),
            Expect::MarketUrl("https://github.com/other/repo.git", NATIVE, true),
            // The add/install gate acquires NEW sources — not grok-native
            // yet, so even an advisory strict list fail-closes it.
            Expect::MarketAddBlocked("https://github.com/other/repo.git", true),
        ],
    );
    assert_expects(
        "grok's own signed TOML layers bind native subjects too",
        &layered(
            None,
            &[(
                PolicyLayerTier::SystemRequirements,
                SYS_REQ,
                r#"
[[denied_mcp_servers]]
server_url = "https://denied.example.com/*"

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/example-corp/approved-plugins.git"
"#,
            )],
        ),
        vec![
            Expect::Allowed("denied", "https://denied.example.com/mcp", NATIVE, false),
            Expect::MarketUrl("https://github.com/other/repo.git", NATIVE, false),
        ],
    );
    assert_expects(
        "a user layer's extra allow URL cannot re-admit what admin excludes",
        &layered(
            Some(serde_json::json!({
                "allowedMcpServers": [ { "serverUrl": "https://admin.example.com/*" } ]
            })),
            &[(
                PolicyLayerTier::UserRequirements,
                USER_REQ,
                r#"
[[allowed_mcp_servers]]
server_url = "https://user.example.com/*"
"#,
            )],
        ),
        vec![
            Expect::Allowed("u", "https://user.example.com/mcp", FOREIGN, false),
            Expect::Allowed("a", "https://admin.example.com/mcp", FOREIGN, false),
        ],
    );
    assert_expects(
        "Claude JSON carries the pins; github+repo strict entries canonicalize",
        &layered(
            Some(serde_json::json!({
                "allowManagedMcpServersOnly": true,
                "enableAllProjectMcpServers": false,
                "strictKnownMarketplaces": [
                    { "source": "github", "repo": "acme/approved-plugins" }
                ],
                "extraKnownMarketplaces": {
                    "acme": {
                        "source": { "source": "github", "repo": "acme/approved-plugins", "ref": "v2" },
                        "autoUpdate": false
                    }
                }
            })),
            &[],
        ),
        vec![
            Expect::ManagedOnly(FOREIGN, true),
            Expect::ProjectMcpPin(Some(CLAUDE_PATH)),
            // github+repo strict entry participates in the allowlist
            // (fail-closed fix: previously the allowlist came out empty).
            Expect::MarketRestricted(true),
            Expect::MarketUrl(
                "https://github.com/acme/approved-plugins.git",
                FOREIGN,
                true,
            ),
            Expect::MarketUrl("https://github.com/evil/repo.git", FOREIGN, false),
            // github+repo extra with ref canonicalizes and keeps the ref.
            Expect::ExtraGit(
                0,
                "acme",
                "https://github.com/acme/approved-plugins.git",
                Some("v2"),
            ),
        ],
    );
    assert_expects(
        "extras-level autoUpdate:false pins global auto-update",
        &layered(
            Some(serde_json::json!({
                "extraKnownMarketplaces": {
                    "corp": {
                        "source": { "source": "git", "url": "https://github.com/corp/approved.git" },
                        "autoUpdate": false
                    }
                }
            })),
            &[],
        ),
        vec![Expect::AutoUpdatePin(Some(CLAUDE_PATH))],
    );
    assert_expects(
        "autoUpdate absent does not pin",
        &layered(
            Some(serde_json::json!({
                "extraKnownMarketplaces": {
                    "corp": { "source": { "source": "git", "url": "https://github.com/corp/approved.git" } }
                }
            })),
            &[],
        ),
        vec![Expect::AutoUpdatePin(None)],
    );
}
/// A wrong-typed extras-level `autoUpdate` can't be honored as an opt-out;
/// it must warn like every other policy bool instead of dropping silently.
#[test]
fn extras_auto_update_wrong_type_warns_and_does_not_pin() {
    let (ms, logs) = capturing_warn_logs(|| {
        layered(
            Some(serde_json::json!({
                "extraKnownMarketplaces": {
                    "corp": {
                        "source": { "source": "git", "url": "https://github.com/corp/approved.git" },
                        "autoUpdate": "false"
                    }
                }
            })),
            &[],
        )
    });
    assert!(
        !ms.plugin_auto_update.is_disabled(),
        "a wrong-typed autoUpdate must not pin"
    );
    assert!(
        logs.contains("policy key must be a boolean"),
        "wrong-typed autoUpdate must warn, got: {logs:?}"
    );
}
/// The full GA fixture: all 43 allowedMcpServers entries parse — zero dropped.
#[test]
fn enterprise_ga_fixture_parses_all_entries() {
    let raw = include_str!("../../../tests/fixtures/enterprise-managed-settings-ga.json");
    let json: serde_json::Value = serde_json::from_str(raw).unwrap();
    let expected = json["allowedMcpServers"].as_array().unwrap().len();
    let path =
        std::path::Path::new("/Library/Application Support/ClaudeCode/managed-settings.json");
    let ms = parse_managed_settings_json(&json, path);
    assert_eq!(ms.mcp_allowlist.sources.len(), 1);
    let source = &ms.mcp_allowlist.sources[0];
    assert_eq!(
        source.entries().count(),
        expected,
        "every allowedMcpServers entry must parse into policy"
    );
    let argv_entries = source
        .entries()
        .filter(|e| matches!(e, AllowedMcpServer::StdioArgv { .. }))
        .count();
    let url_entries = source
        .entries()
        .filter(|e| matches!(e, AllowedMcpServer::Http { .. }))
        .count();
    let fixture_entries_with = |key: &str| {
        json["allowedMcpServers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e.get(key).is_some())
            .count()
    };
    assert_eq!(
        url_entries,
        fixture_entries_with("serverUrl"),
        "every serverUrl entry must parse to an Http entry"
    );
    assert_eq!(
        argv_entries,
        fixture_entries_with("serverCommand"),
        "every serverCommand entry must parse to a StdioArgv entry"
    );
    assert!(ms.mcp_allowlist.managed_only(FOREIGN));
    assert!(ms.project_mcp.is_disabled());
    assert!(ms.plugin_auto_update.is_disabled());
    assert!(ms.mcp_allowlist.is_server_allowed(
        &sa(
            "ui-kit",
            "npx",
            &["@example-corp/ui-kit-mcp", "enterprise-webc"]
        ),
        FOREIGN
    ));
    assert!(ms.mcp_allowlist.is_server_allowed(
        &hs("design", "https://mcp.design-tool.example/mcp"),
        FOREIGN
    ));
    assert!(
        !ms.mcp_allowlist
            .is_server_allowed(&ss("rogue", "python3"), FOREIGN)
    );
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(ms.marketplace_allowlist.is_url_allowed(
        "https://github.com/example-corp/approved-plugins.git",
        FOREIGN
    ));
    assert_eq!(ms.extra_marketplaces.len(), 1);
    assert_eq!(ms.extra_marketplaces[0].name, "approved-plugins");
}
/// Real-file fixture through `managed_config_layers_at` →
/// `managed_toml_policy_layers` → `resolve_managed_settings`: if layer
/// discovery regresses to empty, the policy controls vanish and this fails.
#[test]
fn managed_config_layers_from_disk_reach_the_policy_engine() {
    let system = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        system.path().join("managed_config.toml"),
        r#"
allow_managed_mcp_servers_only = true
plugin_auto_update = false

[[denied_mcp_servers]]
server_url = "https://evil.example.com/*"

[[strict_known_marketplaces]]
source = "git"
url = "https://github.com/corp/approved.git"

[extra_known_marketplaces.corp-local]
source = { source = "local", path = "/opt/mp" }
"#,
    )
    .unwrap();
    std::fs::write(
        user.path().join("managed_config.toml"),
        r#"
[[allowed_mcp_servers]]
server_url = "https://ok.example.com/*"

[extra_known_marketplaces.user-local]
source = { source = "local", path = "/tmp/mp" }
"#,
    )
    .unwrap();
    let layers = xai_grok_config::managed_config_layers_at(Some(system.path()), Some(user.path()));
    let ms = resolve_managed_settings(None, managed_toml_policy_layers(layers, vec![]));
    assert!(
        ms.mcp_allowlist
            .is_server_denied(&hs("evil", "https://evil.example.com/mcp"), NATIVE)
    );
    assert!(ms.mcp_allowlist.managed_only(NATIVE));
    assert!(ms.plugin_auto_update.is_disabled());
    assert_eq!(
        ms.plugin_auto_update.source(),
        Some(system.path().join("managed_config.toml").as_path())
    );
    assert!(ms.marketplace_allowlist.is_restricted());
    assert!(
        !ms.marketplace_allowlist
            .is_url_allowed("https://github.com/evil/repo.git", NATIVE)
    );
    assert!(
        ms.mcp_allowlist
            .is_server_allowed(&hs("ok", "https://ok.example.com/mcp"), NATIVE)
    );
    let ownership: std::collections::HashMap<&str, PolicyLayerOwnership> = ms
        .extra_marketplaces
        .iter()
        .map(|m| (m.name.as_str(), m.ownership))
        .collect();
    assert_eq!(
        ownership.get("corp-local"),
        Some(&PolicyLayerOwnership::Admin)
    );
    assert_eq!(
        ownership.get("user-local"),
        Some(&PolicyLayerOwnership::User)
    );
}
/// Add gate: fail-closed for non-allowlisted git URLs and local paths.
#[test]
fn marketplace_add_gate_fails_closed() {
    let restricted = MarketplacePolicy::single(MarketplaceAllowlist {
        allowed_urls: vec!["https://github.com/example-corp/approved-plugins.git".into()],
        source_path: Some(PathBuf::from(SYS_REQ)),
        authority: PolicySourceAuthority::Native,
    });
    assert!(
        restricted
            .add_block_reason("https://github.com/example-corp/approved-plugins.git")
            .is_none()
    );
    let blocked = restricted
        .add_block_reason("https://github.com/evil/repo.git")
        .expect("non-allowlisted URL blocked");
    assert!(blocked.contains("strictKnownMarketplaces"));
    assert!(
        restricted
            .add_block_reason("/tmp/local-marketplace")
            .is_some()
    );
    let unrestricted = MarketplacePolicy::default();
    assert!(
        unrestricted
            .add_block_reason("https://github.com/any/repo.git")
            .is_none()
    );
    assert!(
        unrestricted
            .add_block_reason("/tmp/local-marketplace")
            .is_none()
    );
}
/// `block_reason` names the source that actually rejects the URL, not the
/// first restricted source in load order.
#[test]
fn marketplace_block_reason_names_the_actual_blocker() {
    let permissive = MarketplaceAllowlist {
        allowed_urls: vec![
            "https://github.com/ok/repo.git".into(),
            "https://github.com/extra/repo.git".into(),
        ],
        source_path: Some(PathBuf::from(CLAUDE_PATH)),
        authority: PolicySourceAuthority::Advisory,
    };
    let strict = MarketplaceAllowlist {
        allowed_urls: vec!["https://github.com/ok/repo.git".into()],
        source_path: Some(PathBuf::from(SYS_REQ)),
        authority: PolicySourceAuthority::Native,
    };
    let policy = MarketplacePolicy {
        sources: vec![permissive, strict],
    };
    let reason = policy
        .add_block_reason("https://github.com/extra/repo.git")
        .expect("blocked by the strict source");
    assert!(
        reason.contains(SYS_REQ),
        "reason must name the blocking source, got: {reason}"
    );
}
#[test]
fn parse_managed_settings_reads_nested_default_mode() {
    let json = serde_json::json!({
        "permissions": {
            "defaultMode": "dontAsk",
            "allow": ["Bash(git status)"]
        }
    });
    let path = std::path::Path::new(CLAUDE_PATH);
    let ms = parse_managed_settings_json(&json, path);
    assert_eq!(ms.default_mode, Some(DefaultPermissionMode::DontAsk));
    assert_eq!(ms.permissions.len(), 1);
    let auto_json = serde_json::json!({
        "permissions": { "defaultMode": "auto" }
    });
    let ms_auto = parse_managed_settings_json(&auto_json, path);
    assert_eq!(ms_auto.default_mode, Some(DefaultPermissionMode::Auto));
}
