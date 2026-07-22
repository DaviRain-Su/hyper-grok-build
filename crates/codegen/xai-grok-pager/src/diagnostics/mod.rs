//! Route-aware terminal diagnostics engine.
//!
//! Warnings are data-only; the engine returns `Vec<TerminalWarning>` for
//! downstream banner rendering.

use std::path::Path;

use crate::notifications::NotificationCondition;
use crate::notifications::protocol::NotificationProtocol;
use crate::terminal::{ByobuBackend, MultiplexerKind, TerminalContext, TerminalName};
use crate::theme::color_support::ColorLevel;

mod doctor_format;
mod fix;
mod model;
pub mod probes;
mod view;

pub use doctor_format::format_doctor;
pub(crate) use fix::human_fix_command;
pub use fix::{
    AutomaticRemediation, FixError, FixOutcome, FixPlan, FixRequest, FixStatus, PlannedChange,
    SSH_WRAP_FIX_COMMAND, SSH_WRAP_ID, SSH_WRAP_ONE_OFF, ShellKind, apply_fix, configured_report,
    managed_alias_configured, plan_fix, resolve_fix_id, ssh_wrap_automatic_remediation,
};
pub(crate) use model::probe_requires_live_tui;
pub use model::{
    ClipboardFacts, ColorFacts, DataControlFact, DiagnosticFacts, DiagnosticFinding, DiagnosticId,
    DiagnosticReport, FindingDisposition, KeyboardFact, ManualRemediation, NewlineFact, ProbeNote,
    ProbeStatus, RuntimeFact, VoiceFacts,
};
pub use view::{DiagnosticSnapshot, view};

/// Passive input-device probe for doctor / `/terminal-setup`.
///
/// Does not open a capture stream (no macOS mic-permission prompt). When
/// `emit_missing_issue` is true and no device exists, appends an issue finding
/// (TUI with voice on). Doctor passes false so headless hosts only show a fact.
pub fn apply_voice_probe(report: &mut DiagnosticReport, emit_missing_issue: bool) {
    if !xai_grok_voice::AUDIO_SUPPORTED {
        return;
    }
    match xai_grok_voice::input_device_info() {
        Ok(device) => {
            report.facts.voice = Some(VoiceFacts::Device {
                name: device.name,
                detail: device.detail,
            });
        }
        Err(err) => {
            let error = match err {
                xai_grok_voice::VoiceError::Config(message) => message,
                other => other.to_string(),
            };
            report.facts.voice = Some(VoiceFacts::Missing {
                error: error.clone(),
            });
            if emit_missing_issue {
                report.findings.push(DiagnosticFinding {
                    id: DiagnosticId::new("voice", "no-input-device"),
                    disposition: FindingDisposition::Issue,
                    message: format!("Voice dictation can't capture audio — {error}"),
                    remediation: None,
                    automatic_remediation: None,
                    note: Some(
                        "Connect or select an input device in your system sound settings, then \
                         re-run /terminal-setup or grok doctor."
                            .to_owned(),
                    ),
                });
            }
        }
    }
}

/// Broad classification of a startup warning.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WarningCategory {
    /// OSC 52 clipboard passthrough is misconfigured in tmux.
    Clipboard,
    /// DCS passthrough is disabled (nested clipboard path).
    DcsPassthrough,
    /// tmux control-mode degrades the fullscreen experience.
    ControlMode,
    /// The session is running inside Byobu backed by GNU screen — best-effort
    /// support only.
    ByobuScreen,
    /// The terminal emulator (Apple Terminal.app) does not support OSC 52
    /// clipboard escape sequences.
    UnsupportedTerminal,
    /// tmux 3.3+ has `extended-keys` set to `off`, so kitty CSI-u responses
    /// would be stripped before they reach the pager.
    TmuxExtendedKeysOff,
    /// The terminal is unrecognised and the notification protocol fell back to
    /// BEL (audible bell only).
    NotificationProtocolFallback,
    /// The terminal does not reliably support CSI focus-tracking events, so
    /// `condition = "unfocused"` will never fire.
    FocusTrackingUnavailable,
    /// WezTerm with the Kitty keyboard protocol inactive (its
    /// `enable_kitty_keyboard` option defaults to `false`), so Shift+Enter
    /// is byte-identical to Enter and can't insert newlines — and WezTerm's
    /// default Alt+Enter binding (ToggleFullScreen) eats the fallback chord.
    WezTermKittyKeyboardOff,
    /// Wayland session whose compositor lacks the data-control clipboard
    /// protocol (GNOME ≤ 47), so every native copy rides a focus-dependent
    /// path (arboard via the XWayland selection bridge, `wl-copy`'s
    /// no-data-control fallback) and fails if the terminal loses focus
    /// mid-copy.
    WaylandNoDataControl,
    /// Below truecolor: truecolor themes hidden. Explicit `/doctor` only.
    LimitedColorSupport,
    SandboxProfileConflict,
    /// The session runs over SSH without `grok wrap` on the local end, so
    /// clipboard forwarding and terminal-mode restore on dropped connections
    /// are not guaranteed. Informational recommendation, not a breakage.
    SshWithoutWrap,
}

/// A structured startup warning carrying category, human-readable description,
/// and optional fix guidance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalWarning {
    /// The warning category for downstream grouping and filtering.
    pub category: WarningCategory,
    /// A short human-readable description of the problem.
    pub message: String,
    /// Optional fix instruction (e.g. a tmux config line to add).
    pub fix: Option<String>,
    /// The config file path where the fix should be applied, when applicable.
    pub config_path: Option<String>,
    /// Optional note about trade-offs or side effects of the fix.
    pub note: Option<String>,
}

impl TerminalWarning {
    /// Create a new warning with all fields.
    fn new(
        category: WarningCategory,
        message: &str,
        fix: Option<&str>,
        config_path: Option<&str>,
    ) -> Self {
        Self {
            category,
            message: message.to_string(),
            fix: fix.map(|s| s.to_string()),
            config_path: config_path.map(|s| s.to_string()),
            note: None,
        }
    }
}

/// Summarize a list of terminal warnings into a single [`StartupWarning`]
/// for the welcome screen. Returns `None` if there are no warnings or if
/// none of the warnings are in the allow-list of safe-to-surface categories.
///
/// The welcome screen doesn't need to know what's wrong -- only that
/// something is wrong and where to go for details.
pub fn summarize_warnings(
    warnings: &[TerminalWarning],
    is_ssh: bool,
) -> Option<crate::startup::StartupWarning> {
    if !is_ssh {
        return None;
    }
    summarize_warnings_inner(warnings)
}

fn summarize_warnings_inner(
    warnings: &[TerminalWarning],
) -> Option<crate::startup::StartupWarning> {
    // Allow-list of categories where detection is a direct tmux subprocess
    // query that only triggers on an explicit non-good value, and the fix is
    // a single config line. Other categories stay suppressed until their
    // false-positive rate is characterized.
    warnings.iter().find(|w| {
        matches!(
            w.category,
            WarningCategory::TmuxExtendedKeysOff | WarningCategory::DcsPassthrough
        )
    })?;
    Some(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Warning,
        message: "Clipboard may be unreachable.".to_string(),
        action: Some("See /terminal-setup for potential fixes.".to_string()),
    })
}

/// Collect all applicable startup warnings for the current terminal context.
///
/// This is the primary entry point for the diagnostics engine. It returns
/// structured warnings as data — no stderr output, no sleep, no side effects.
///
pub fn collect_startup_warnings(snapshot: &probes::ProbeSnapshot<'_>) -> Vec<TerminalWarning> {
    collect_startup_warnings_from(
        snapshot.terminal,
        &snapshot.tmux,
        Some(snapshot.runtime.fullscreen_active),
    )
}

pub(crate) fn collect_startup_warnings_from(
    ctx: &TerminalContext,
    tmux: &probes::TmuxProbeFacts,
    fullscreen_active: Option<bool>,
) -> Vec<TerminalWarning> {
    let mut warnings = Vec::new();

    // Apple Terminal.app does not support OSC 52. Over SSH, this means
    // clipboard writes can never reach the user's local machine.
    if ctx.brand == TerminalName::AppleTerminal && ctx.is_ssh {
        warnings.push(TerminalWarning::new(
            WarningCategory::UnsupportedTerminal,
            "macOS Terminal does not support clipboard escape sequences (OSC 52) \
             -- copy over SSH will not work.",
            None,
            None,
        ));
    }

    // Byobu-on-screen: best-effort warning, no further tmux-specific checks.
    if ctx.byobu == Some(ByobuBackend::Screen) {
        warnings.push(TerminalWarning::new(
            WarningCategory::ByobuScreen,
            "Byobu with GNU screen backend -- clipboard and display support is best-effort",
            None,
            None,
        ));
        return warnings;
    }

    // tmux control-mode warning — the message reflects the effective
    // fullscreen state so callers that force fullscreen in control mode
    // (e.g. `alt_screen = "always"`) see an accurate warning rather than a
    // blanket "inline mode" claim.
    if ctx.is_tmux_backed() && matches!(tmux.control_mode, probes::TmuxProbeResult::Available(true))
    {
        let message = match fullscreen_active {
            Some(true) => {
                "tmux control mode detected -- fullscreen may be unreliable in control mode"
            }
            Some(false) => "tmux control mode detected -- running in degraded inline mode",
            None => "tmux control mode detected -- terminal display may be degraded",
        };
        warnings.push(TerminalWarning::new(
            WarningCategory::ControlMode,
            message,
            None,
            None,
        ));
    }

    // Resolve tmux config path once for all tmux-related warnings below.
    let config_path = ctx.tmux_config_path();

    // tmux-backed clipboard and DCS passthrough checks.
    if ctx.is_tmux_backed() {
        warnings.extend(diagnose_clipboard_from_facts(tmux, &config_path));
    }

    if ctx.kitty_skip_reason() == Some("tmux_extended_keys_off") {
        let mut warning = TerminalWarning::new(
            WarningCategory::TmuxExtendedKeysOff,
            "tmux extended-keys is off -- modifier key combinations may not reach the pager",
            Some("set -g extended-keys on"),
            Some(&config_path),
        );
        // Existing tmux sessions cache the option; without an explicit
        // reload the user will edit the config, see no change, and
        // conclude the fix is broken.
        warning.note = Some(format!(
            "Then reload tmux: `tmux source-file {config_path}` (or detach and reattach)."
        ));
        warnings.push(warning);
    }

    warnings
}

/// Warn when WezTerm is running without the Kitty keyboard protocol.
///
/// WezTerm ships `enable_kitty_keyboard = false` by default, so the pager's
/// runtime probe fails and no enhancement flags are pushed. Without KKP,
/// Shift+Enter arrives as a bare `CR` (submits instead of inserting a
/// newline), and WezTerm's default Alt+Enter binding (ToggleFullScreen)
/// swallows the usual fallback chord before it reaches the PTY.
///
/// WezTerm is recognized two ways:
/// - env detection (`ctx.brand`) for local sessions, and
/// - the async XTVERSION self-report (`xtversion_payload`) for SSH
///   sessions, where `TERM_PROGRAM` isn't forwarded and the brand falls
///   back to `Unknown`. The reply arrives through the event loop after
///   startup, so this path lights up for `/doctor` (and any warning pass
///   re-run after the reply landed) rather than the very
///   first startup banner.
///
/// `kitty_flags_pushed` is the runtime negotiation outcome from
/// `init_terminal` (passed in so this stays a pure, testable function).
/// Returns `None` when KKP is active, when the terminal isn't WezTerm, or
/// when a non-WezTerm [`TerminalContext::kitty_skip_reason`] applies (e.g.
/// tmux) — in that case the wezterm.lua fix alone wouldn't help and other
/// warnings cover it.
pub fn wezterm_kitty_keyboard_warning(
    snapshot: &probes::ProbeSnapshot<'_>,
) -> Option<TerminalWarning> {
    wezterm_kitty_keyboard_warning_from(
        snapshot.terminal,
        snapshot.runtime.kitty_flags_pushed,
        snapshot.runtime.xtversion,
    )
}

pub(crate) fn wezterm_shape(
    ctx: &TerminalContext,
    xtversion_payload: Option<&str>,
) -> Option<WezTermShape> {
    if ctx.brand == TerminalName::WezTerm {
        return Some(WezTermShape::Environment);
    }
    (ctx.brand == TerminalName::Unknown
        && ctx.multiplexer == MultiplexerKind::Undetected
        && ctx.is_ssh
        && xtversion_payload.is_some_and(|v| v.trim_start().starts_with("WezTerm")))
    .then_some(WezTermShape::SshXtversion)
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum WezTermShape {
    Environment,
    SshXtversion,
}

pub(crate) fn wezterm_kitty_keyboard_warning_from(
    ctx: &TerminalContext,
    kitty_flags_pushed: bool,
    xtversion_payload: Option<&str>,
) -> Option<TerminalWarning> {
    let shape = wezterm_shape(ctx, xtversion_payload)?;
    if kitty_flags_pushed {
        return None;
    }
    if shape == WezTermShape::Environment && ctx.kitty_skip_reason().is_some() {
        return None;
    }
    if shape == WezTermShape::SshXtversion {
        let mut warning = TerminalWarning::new(
            WarningCategory::WezTermKittyKeyboardOff,
            "WezTerm over SSH: Shift+Enter can't insert newlines",
            None,
            None,
        );
        warning.note = Some(
            "Type `\\` then Enter to insert a newline. The pager doesn't negotiate the \
             kitty keyboard protocol over SSH yet; `enable_kitty_keyboard = true` in \
             wezterm.lua fixes local WezTerm sessions only."
                .to_string(),
        );
        return Some(warning);
    }
    let mut warning = TerminalWarning::new(
        WarningCategory::WezTermKittyKeyboardOff,
        "WezTerm: Shift+Enter can't insert newlines (kitty keyboard protocol is off)",
        Some("config.enable_kitty_keyboard = true"),
        Some("~/.config/wezterm/wezterm.lua"),
    );
    warning.note = Some(
        "Restart WezTerm after the change. Until then, type `\\` then Enter to insert \
         a newline."
            .to_string(),
    );
    Some(warning)
}

pub fn sandbox_profile_conflict_warning(workspace: &Path) -> Option<TerminalWarning> {
    sandbox_profile_conflict_warning_from(xai_grok_sandbox::sandbox_profile_conflicts(workspace))
}

fn sandbox_profile_conflict_warning_from(conflicts: Vec<String>) -> Option<TerminalWarning> {
    if conflicts.is_empty() {
        return None;
    }
    let profiles = conflicts
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(TerminalWarning {
        category: WarningCategory::SandboxProfileConflict,
        message: format!(
            "Your project sandbox profile conflicts with user config.\nProfile: {profiles}\nProject config: .grok/sandbox.toml\nUser config: ~/.grok/sandbox.toml"
        ),
        fix: Some("Using the user profile instead.".to_string()),
        config_path: None,
        note: None,
    })
}

/// Pure SSH `grok wrap` recommendation — suggests launching the session
/// through `grok wrap ssh <host>` on the user's local machine, which gives a
/// remote session reliable clipboard forwarding plus terminal-mode restore
/// when the connection drops.
///
/// Gates (all must hold):
/// - `is_ssh` — the session runs over SSH ([`TerminalContext::is_ssh`]);
/// - `!osc52_sink_active` — no wrap is already capturing our output. `grok
///   wrap` advertises its OSC 52 sink through the SSH hop via an env var
///   (see `clipboard::osc52_sink_active`), so once a user adopts wrap the
///   hint silences itself with no further bookkeeping. Env-based, so stale
///   under tmux (panes inherit the server's env at server start): a server
///   started before wrap misses the sink and the hint fires despite wrap,
///   and one started under wrap keeps suppressing after wrap is gone —
///   accepted, the same exposure the SSH env checks already live with;
/// - `!is_official_vscode_remote` — a VS Code remote integrated terminal is
///   not a plain ssh terminal the user could wrap.
///
/// This detector describes environment shape only; the
/// `[ui.contextual_hints].ssh_wrap` policy gate is applied by the ephemeral
/// tip's trigger (`AppView::maybe_trigger_ssh_wrap_tip`), while
/// `/doctor` deliberately lists the recommendation unconditionally.
/// All inputs are injected so tests never touch ambient env (pattern:
/// [`diagnose_wayland_data_control`]).
pub fn ssh_wrap_hint(
    is_ssh: bool,
    osc52_sink_active: bool,
    is_official_vscode_remote: bool,
) -> Option<TerminalWarning> {
    if !is_ssh || osc52_sink_active || is_official_vscode_remote {
        return None;
    }
    let mut warning = TerminalWarning::new(
        WarningCategory::SshWithoutWrap,
        "Running over SSH without `grok wrap` -- clipboard copies depend on the \
         terminal's escape-sequence support, and a dropped connection can leave \
         your local terminal in a bad state",
        Some("grok wrap ssh <host>"),
        None,
    );
    warning.note = Some(
        "Run it on your local machine in place of plain `ssh` -- it forwards \
         clipboard copies to your local system and restores terminal modes if \
         the connection drops."
            .to_string(),
    );
    Some(warning)
}

/// Assemble the welcome-screen startup warning list.
///
/// The welcome screen renders a single entry — the severity-aware pick from
/// `startup::banner_warning`, whose doc owns the selection contract — so
/// assemble order decides precedence among Warnings. The WezTerm kitty-keyboard
/// warning (when present) goes **first**: a broken-local-input warning
/// outranks the SSH clipboard advisories from [`summarize_warnings`]. The
/// Wayland no-data-control warning follows the same bypass (surfaced locally
/// — [`summarize_warnings`] is SSH-gated — but after WezTerm: broken input
/// outranks focus-dependent copies). Keeping the banner copy here (instead of
/// at the call site) ties it to the warnings so the surfaces can't drift.
pub fn assemble_startup_warnings(
    wezterm_warning: Option<&TerminalWarning>,
    wayland_clipboard_warning: Option<&TerminalWarning>,
    sandbox_profile_warning: Option<&TerminalWarning>,
    mut summarized: Vec<crate::startup::StartupWarning>,
) -> Vec<crate::startup::StartupWarning> {
    if let Some(w) = sandbox_profile_warning {
        summarized.insert(
            0,
            crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: w.message.clone(),
                action: w.fix.clone(),
            },
        );
    }
    if wayland_clipboard_warning.is_some() {
        summarized.insert(
            0,
            crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: "Copies need this terminal to stay focused.".to_string(),
                action: Some("See /terminal-setup for details.".to_string()),
            },
        );
    }
    if wezterm_warning.is_some() {
        summarized.insert(
            0,
            crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: "Shift+Enter newlines need a WezTerm config change.".to_string(),
                action: Some("See /terminal-setup for the fix.".to_string()),
            },
        );
    }
    summarized
}

/// Returns `true` if the terminal brand is known to support CSI focus-tracking
/// events (`\x1b[?1004h` / focus-in / focus-out sequences).
fn supports_focus_tracking(brand: TerminalName) -> bool {
    brand != TerminalName::AppleTerminal && !brand.is_capability_unclassified()
}

/// Collect notification-specific startup warnings.
///
/// These complement the general terminal warnings from
/// [`collect_startup_warnings`] and depend on the resolved notification
/// protocol and condition.
pub fn collect_notification_warnings(
    snapshot: &probes::ProbeSnapshot<'_>,
    protocol: NotificationProtocol,
    condition: NotificationCondition,
) -> Vec<TerminalWarning> {
    let ctx = snapshot.terminal;
    let mut warnings = Vec::new();

    // Protocol fallback: BEL selected for an unknown terminal in auto mode.
    if protocol == NotificationProtocol::Bel && ctx.brand == TerminalName::Unknown {
        warnings.push(TerminalWarning::new(
            WarningCategory::NotificationProtocolFallback,
            "notification protocol fell back to BEL -- terminal not recognized",
            None,
            None,
        ));
    }

    // tmux + OSC protocol: allow-passthrough must be on or OSC notification
    // sequences wrapped in DCS passthrough will be silently dropped.
    if ctx.is_tmux_backed()
        && matches!(
            protocol,
            NotificationProtocol::Osc9 | NotificationProtocol::Osc99 | NotificationProtocol::Osc777
        )
        && let probes::TmuxProbeResult::Available(val) = &snapshot.tmux.allow_passthrough
        && !matches!(val.as_str(), "on" | "all")
    {
        let config_path = ctx.tmux_config_path();
        warnings.push(TerminalWarning::new(
            WarningCategory::DcsPassthrough,
            "tmux allow-passthrough is off -- OSC notifications will not reach the terminal",
            Some("set -g allow-passthrough on"),
            Some(&config_path),
        ));
    }

    // Focus tracking: if the terminal doesn't support it and the condition
    // is "unfocused", notifications will never fire because the pager will
    // always think the window is focused.
    if condition == NotificationCondition::Unfocused && !supports_focus_tracking(ctx.brand) {
        warnings.push(TerminalWarning::new(
            WarningCategory::FocusTrackingUnavailable,
            "focus tracking may not be supported -- unfocused notifications may not fire",
            Some("set condition = \"always\" in [ui.notifications]"),
            None,
        ));
    }

    warnings
}

fn diagnose_clipboard_from_facts(
    tmux: &probes::TmuxProbeFacts,
    config_path: &str,
) -> Vec<TerminalWarning> {
    let set_clipboard = match &tmux.set_clipboard {
        probes::TmuxProbeResult::Available(value) => Some(value.as_str()),
        _ => None,
    };
    let passthrough_exists = !matches!(
        tmux.allow_passthrough_support,
        probes::TmuxProbeResult::Unsupported
    );
    let allow_passthrough = match &tmux.allow_passthrough {
        probes::TmuxProbeResult::Available(value) => Some(value.as_str()),
        _ => None,
    };
    diagnose_clipboard_from_values(
        set_clipboard,
        passthrough_exists,
        allow_passthrough,
        config_path,
    )
}

/// Pure clipboard diagnostic logic — determines which tmux clipboard settings
/// are misconfigured and returns structured warnings.
///
/// **Query-failure semantics:** `None` for `set_clipboard` or
/// `allow_passthrough` means the tmux query could not obtain a value (e.g. the
/// tmux server is unreachable or the option is unset).  This is *not* proof
/// that the setting is disabled, so `None` does **not** trigger a
/// misconfiguration warning.  Only an explicit non-good value (e.g. `"off"`)
/// produces a warning with remediation guidance.
///
/// - `set_clipboard`: value of `set-clipboard` (`None` = query unavailable).
/// - `passthrough_exists`: whether the tmux server knows the `allow-passthrough`
///   option (introduced in tmux 3.3; older versions don't have it).
/// - `allow_passthrough`: value of `allow-passthrough` when it exists.
/// - `config_path`: the tmux config file path for fix guidance.
pub fn diagnose_clipboard_from_values(
    set_clipboard: Option<&str>,
    passthrough_exists: bool,
    allow_passthrough: Option<&str>,
    config_path: &str,
) -> Vec<TerminalWarning> {
    let mut warnings = Vec::new();

    // set-clipboard: required for OSC 52 passthrough so the pager can write to
    // the user's local clipboard.
    //
    // `None` = query failed or value unavailable → do not claim it is disabled.
    // Only warn when the query returned an explicit non-good value.
    if let Some(val) = set_clipboard
        && !matches!(val, "on" | "external")
    {
        warnings.push(TerminalWarning::new(
            WarningCategory::Clipboard,
            "OSC 52 clipboard passthrough is disabled",
            Some("set -g set-clipboard on"),
            Some(config_path),
        ));
    }

    // allow-passthrough: needed for DCS passthrough of OSC 52 in nested tmux.
    // This option was introduced in tmux 3.3. Before 3.3, DCS passthrough
    // worked unconditionally, so we only warn when the option actually exists.
    //
    // Same query-failure semantics: `None` does not produce a warning.
    if passthrough_exists
        && let Some(val) = allow_passthrough
        && !matches!(val, "on" | "all")
    {
        warnings.push(TerminalWarning::new(
            WarningCategory::DcsPassthrough,
            "DCS passthrough is disabled (needed for nested clipboard)",
            Some("set -g allow-passthrough on"),
            Some(config_path),
        ));
    }

    warnings
}

/// Pure Wayland clipboard diagnostic — flags the focus-dependent copy shape.
///
/// Warns only when the session is Wayland AND the compositor lacks the
/// data-control protocol: without it every native write (arboard via the
/// XWayland bridge, `wl-copy`'s fallback) needs the terminal focused until the
/// write completes, so alt-tabbing mid-copy loses the copy. When `wl-copy` is
/// also missing, the fix suggests installing wl-clipboard — a partial
/// mitigation (its verified write is the most reliable non-data-control
/// route). No warning when data-control is present (copies are focus-free) or
/// off Wayland.
pub fn diagnose_wayland_data_control(
    is_wayland: bool,
    data_control: bool,
    wl_copy_available: bool,
) -> Option<TerminalWarning> {
    if !is_wayland || data_control {
        return None;
    }
    let fix = (!wl_copy_available)
        .then_some("sudo apt install wl-clipboard  (or your distro's equivalent)");
    Some(TerminalWarning::new(
        WarningCategory::WaylandNoDataControl,
        "Wayland compositor without the data-control clipboard protocol -- \
         keep the terminal focused while copying until the copy toast confirms",
        fix,
        None,
    ))
}

pub fn diagnose_wayland_data_control_from_snapshot(
    snapshot: &probes::ProbeSnapshot<'_>,
) -> Option<TerminalWarning> {
    diagnose_wayland_data_control(
        snapshot.wayland.is_wayland,
        matches!(
            snapshot.wayland.data_control,
            probes::TmuxProbeResult::Available(true)
        ),
        snapshot.wayland.wl_copy_available,
    )
}

pub(crate) fn diagnose_wayland_data_control_from_common(
    snapshot: &probes::CommonProbeSnapshot<'_>,
) -> Option<TerminalWarning> {
    let probes::TmuxProbeResult::Available(data_control) = snapshot.wayland.data_control else {
        return None;
    };
    diagnose_wayland_data_control(
        snapshot.wayland.is_wayland,
        data_control,
        snapshot.wayland.wl_copy_available,
    )
}

#[derive(Clone, Copy, Debug)]
pub struct ClipboardDiagnosticsInput<'a> {
    pub route_native: bool,
    pub route_tmux: bool,
    pub route_osc52: bool,
    pub native_tool: &'a str,
    pub brand: TerminalName,
    pub host_os: crate::host::HostOs,
    pub display_server: crate::host::DisplayServer,
    pub is_ssh: bool,
    pub container_no_display: bool,
    pub osc52_sink: bool,
    pub wayland_data_control: bool,
    pub wl_copy_available: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ClipboardDiagnostics {
    pub text: String,
    pub has_issue: bool,
}

/// Format preflight clipboard routes without claiming that a copy already happened.
pub fn format_clipboard_diagnostics(input: ClipboardDiagnosticsInput<'_>) -> ClipboardDiagnostics {
    use crate::clipboard::{
        ClipboardDelivery, ClipboardEnvironment, NativeClipboardPreflight, expected_delivery,
        native_clipboard_preflight,
    };

    let environment = ClipboardEnvironment {
        brand: input.brand,
        host_os: input.host_os,
        display_server: input.display_server,
        remote: input.is_ssh,
        container: input.container_no_display,
        osc52_sink: input.osc52_sink,
        wayland_data_control: input.wayland_data_control,
        wl_copy_available: input.wl_copy_available,
    };
    let capability = environment.osc52_capability();
    let native_preflight = native_clipboard_preflight(input.route_native, environment);
    let delivery = expected_delivery(
        native_preflight,
        input.route_tmux,
        input.route_osc52,
        environment,
    );
    let native = match native_preflight {
        NativeClipboardPreflight::LocalAvailable => format!("local ({})", input.native_tool),
        NativeClipboardPreflight::RemoteOnly if input.container_no_display => {
            format!("container ({})", input.native_tool)
        }
        NativeClipboardPreflight::RemoteOnly => format!("remote ({})", input.native_tool),
        NativeClipboardPreflight::Unavailable => "unavailable".to_owned(),
        NativeClipboardPreflight::Disabled => "off".to_owned(),
    };
    let tmux = if input.route_tmux { "on" } else { "off" };
    let osc52 = if input.route_osc52 {
        capability.label()
    } else {
        "off"
    };
    let wrap = if input.osc52_sink { "on" } else { "off" };
    let status = match delivery {
        ClipboardDelivery::Confirmed => "confirmed",
        ClipboardDelivery::Unverified => "unverified",
        ClipboardDelivery::Failed => "unavailable",
    };
    let fix = match delivery {
        ClipboardDelivery::Confirmed => None,
        ClipboardDelivery::Unverified if input.is_ssh => {
            Some("grok wrap <ssh command> or /minimal")
        }
        ClipboardDelivery::Unverified if input.container_no_display => {
            Some("grok wrap <command> or /minimal")
        }
        ClipboardDelivery::Unverified => Some("grok wrap or /minimal"),
        ClipboardDelivery::Failed if input.is_ssh => Some("grok wrap <ssh command> or /minimal"),
        ClipboardDelivery::Failed if input.container_no_display => {
            Some("grok wrap <command> or /minimal")
        }
        ClipboardDelivery::Failed => Some("/minimal"),
    };

    let mut out = String::from("Clipboard\n");
    out.push_str(&format!("  native       {native}\n"));
    out.push_str(&format!("  tmux         {tmux}\n"));
    out.push_str(&format!("  osc 52       {osc52}\n"));
    out.push_str(&format!("  wrap         {wrap}\n"));
    if input.display_server == crate::host::DisplayServer::Wayland {
        out.push_str(&format!(
            "  data-control {}\n",
            if input.wayland_data_control {
                "on"
            } else {
                "off"
            }
        ));
    }
    out.push_str(&format!("  status       {status}\n"));
    if let Some(fix) = fix {
        out.push_str(&format!("  fix          {fix}\n"));
    }
    ClipboardDiagnostics {
        text: out,
        has_issue: !delivery.is_confirmed(),
    }
}

/// Explicit `/doctor` warning when truecolor themes are locked out.
///
/// Not in `collect_startup_warnings` — limited color is normal on some
/// emulators and would spam the welcome banner.
pub fn color_support_warning(
    level: ColorLevel,
    brand: TerminalName,
    is_tmux_backed: bool,
    tmux_config_path: &str,
) -> Option<TerminalWarning> {
    if level.has_truecolor() {
        return None;
    }

    if level == ColorLevel::None {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            "NO_COLOR set -- themed colors disabled",
            None,
            None,
        );
        warning.note = Some("Unset NO_COLOR and restart Grok.".to_string());
        return Some(warning);
    }

    let level_label = level.as_str();

    if brand == TerminalName::AppleTerminal {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            "Terminal.app is 256-color -- truecolor themes unavailable",
            None,
            None,
        );
        warning.note = Some("Switch to a truecolor terminal (e.g. Ghostty).".to_string());
        return Some(warning);
    }

    if is_tmux_backed {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            &format!("Color level is {level_label} -- truecolor themes unavailable"),
            Some("set -as terminal-features \",*:RGB\""),
            Some(tmux_config_path),
        );
        warning.note = Some(format!(
            "Also: set -g default-terminal \"tmux-256color\"; export COLORTERM=truecolor; \
             then `tmux source-file {tmux_config_path}`."
        ));
        return Some(warning);
    }

    let mut warning = TerminalWarning::new(
        WarningCategory::LimitedColorSupport,
        &format!("Color level is {level_label} -- truecolor themes unavailable"),
        Some("export COLORTERM=truecolor"),
        None,
    );
    warning.note = Some("Persist in ~/.zshrc / ~/.bashrc and restart Grok.".to_string());
    Some(warning)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        ByobuBackend, MultiplexerKind, TerminalContext, TerminalName, TmuxClientMeta,
    };

    struct FakeTmuxQuery {
        set_clipboard: Option<String>,
        allow_passthrough_exists: bool,
        allow_passthrough: Option<String>,
        error: Option<String>,
    }

    impl FakeTmuxQuery {
        /// Create a healthy modern tmux fixture (all options correct).
        fn healthy_modern() -> Self {
            Self {
                set_clipboard: Some("on".to_owned()),
                allow_passthrough_exists: true,
                allow_passthrough: Some("on".to_owned()),
                error: None,
            }
        }

        /// Create a fixture where no tmux server is reachable.
        fn unavailable() -> Self {
            Self {
                set_clipboard: None,
                allow_passthrough_exists: false,
                allow_passthrough: None,
                error: None,
            }
        }

        fn error() -> Self {
            Self {
                set_clipboard: None,
                allow_passthrough_exists: true,
                allow_passthrough: None,
                error: Some("tmux unavailable".to_owned()),
            }
        }
    }

    impl probes::TmuxOptionQuery for FakeTmuxQuery {
        fn show_option(&self, option: &str) -> probes::TmuxProbeResult<String> {
            if let Some(error) = &self.error {
                return probes::TmuxProbeResult::Error(error.clone());
            }
            match option {
                "set-clipboard" => self
                    .set_clipboard
                    .clone()
                    .map(probes::TmuxProbeResult::Available)
                    .unwrap_or(probes::TmuxProbeResult::Unavailable),
                "allow-passthrough" if !self.allow_passthrough_exists => {
                    probes::TmuxProbeResult::Unsupported
                }
                "allow-passthrough" => self
                    .allow_passthrough
                    .clone()
                    .map(probes::TmuxProbeResult::Available)
                    .unwrap_or(probes::TmuxProbeResult::Unavailable),
                _ => probes::TmuxProbeResult::Unsupported,
            }
        }

        fn option_support(&self, option: &str) -> probes::TmuxProbeResult<()> {
            if let Some(error) = &self.error {
                return probes::TmuxProbeResult::Error(error.clone());
            }
            match option {
                "allow-passthrough" if self.allow_passthrough_exists => {
                    probes::TmuxProbeResult::Available(())
                }
                "allow-passthrough" => probes::TmuxProbeResult::Unsupported,
                _ => probes::TmuxProbeResult::Unsupported,
            }
        }

        fn control_mode(&self) -> probes::TmuxProbeResult<bool> {
            probes::TmuxProbeResult::Unavailable
        }
    }

    fn test_snapshot<'a>(
        ctx: &'a TerminalContext,
        query: &dyn probes::TmuxOptionQuery,
        control_mode: bool,
        fullscreen_active: bool,
        kitty_flags_pushed: bool,
        xtversion: Option<&'a str>,
    ) -> probes::ProbeSnapshot<'a> {
        probes::ProbeSnapshot {
            terminal: ctx,
            tmux: probes::TmuxProbeFacts {
                version: probes::TmuxProbeResult::Unavailable,
                extended_keys: probes::TmuxProbeResult::Unavailable,
                set_clipboard: query.show_option("set-clipboard"),
                allow_passthrough_support: query.option_support("allow-passthrough"),
                allow_passthrough: query.show_option("allow-passthrough"),
                control_mode: probes::TmuxProbeResult::Available(control_mode),
            },
            wayland: probes::WaylandProbeFacts {
                is_wayland: false,
                data_control: probes::TmuxProbeResult::Available(false),
                wl_copy_available: false,
            },
            runtime: probes::TuiProbeEvidence {
                fullscreen_active,
                kitty_flags_pushed,
                xtversion,
            },
        }
    }

    fn collect_startup_warnings(
        ctx: &TerminalContext,
        query: &dyn probes::TmuxOptionQuery,
        control_mode: bool,
        fullscreen_active: bool,
    ) -> Vec<TerminalWarning> {
        super::collect_startup_warnings(&test_snapshot(
            ctx,
            query,
            control_mode,
            fullscreen_active,
            false,
            None,
        ))
    }

    fn wezterm_kitty_keyboard_warning(
        ctx: &TerminalContext,
        kitty_flags_pushed: bool,
        xtversion: Option<&str>,
    ) -> Option<TerminalWarning> {
        let query = FakeTmuxQuery::unavailable();
        super::wezterm_kitty_keyboard_warning(&test_snapshot(
            ctx,
            &query,
            false,
            true,
            kitty_flags_pushed,
            xtversion,
        ))
    }

    fn collect_notification_warnings(
        ctx: &TerminalContext,
        protocol: NotificationProtocol,
        condition: NotificationCondition,
        query: &dyn probes::TmuxOptionQuery,
    ) -> Vec<TerminalWarning> {
        super::collect_notification_warnings(
            &test_snapshot(ctx, query, false, true, false, None),
            protocol,
            condition,
        )
    }

    // -- Test context builders ------------------------------------------------

    fn plain_terminal_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Ghostty,
            ..Default::default()
        }
    }

    fn plain_tmux_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Iterm2,
            multiplexer: MultiplexerKind::Tmux,
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%0".to_owned()),
            },
            ..Default::default()
        }
    }

    fn byobu_tmux_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Tmux,
            byobu: Some(ByobuBackend::Tmux),
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%1".to_owned()),
            },
            ..Default::default()
        }
    }

    fn byobu_screen_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Screen,
            byobu: Some(ByobuBackend::Screen),
            ..Default::default()
        }
    }

    fn plain_screen_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Screen,
            ..Default::default()
        }
    }

    fn apple_terminal_ctx(is_ssh: bool) -> TerminalContext {
        TerminalContext {
            brand: TerminalName::AppleTerminal,
            is_ssh,
            ..Default::default()
        }
    }

    fn zellij_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Ghostty,
            multiplexer: MultiplexerKind::Zellij,
            ..Default::default()
        }
    }

    // =====================================================================
    // diagnose_clipboard_from_values: pure clipboard logic
    // =====================================================================

    fn clipboard_input(brand: TerminalName) -> ClipboardDiagnosticsInput<'static> {
        ClipboardDiagnosticsInput {
            route_native: true,
            route_tmux: false,
            route_osc52: true,
            native_tool: "arboard",
            brand,
            host_os: crate::host::HostOs::Linux,
            display_server: crate::host::DisplayServer::Unknown,
            is_ssh: true,
            container_no_display: false,
            osc52_sink: false,
            wayland_data_control: false,
            wl_copy_available: false,
        }
    }

    #[test]
    fn clipboard_diagnostics_unknown_ssh_is_unverified() {
        let diagnostics = format_clipboard_diagnostics(clipboard_input(TerminalName::Unknown));
        for expected in [
            "Clipboard",
            "native       remote (arboard)",
            "tmux         off",
            "osc 52       unknown",
            "wrap         off",
            "status       unverified",
            "fix          grok wrap <ssh command> or /minimal",
        ] {
            assert!(
                diagnostics.text.contains(expected),
                "missing {expected:?}:\n{}",
                diagnostics.text
            );
        }
        assert!(diagnostics.has_issue);
    }

    #[test]
    fn clipboard_diagnostics_known_terminal_status() {
        let supported = format_clipboard_diagnostics(clipboard_input(TerminalName::Ghostty));
        assert!(supported.text.contains("osc 52       supported"));
        assert!(supported.text.contains("status       confirmed"));
        assert!(!supported.has_issue);

        let unsupported = format_clipboard_diagnostics(clipboard_input(TerminalName::Vte));
        assert!(unsupported.text.contains("osc 52       unsupported"));
        assert!(unsupported.text.contains("status       unavailable"));
        assert!(
            unsupported
                .text
                .contains("fix          grok wrap <ssh command> or /minimal")
        );
        assert!(unsupported.has_issue);

        let unsupported_container = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
            is_ssh: false,
            container_no_display: true,
            ..clipboard_input(TerminalName::Vte)
        });
        assert!(
            unsupported_container
                .text
                .contains("osc 52       unsupported")
        );
        assert!(
            unsupported_container
                .text
                .contains("status       unavailable")
        );
    }

    #[test]
    fn clipboard_diagnostics_local_wayland_native_matrix() {
        for (data_control, wl_copy, expected) in [
            (false, false, crate::clipboard::ClipboardDelivery::Failed),
            (false, true, crate::clipboard::ClipboardDelivery::Confirmed),
            (true, false, crate::clipboard::ClipboardDelivery::Confirmed),
        ] {
            let diagnostics = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
                route_osc52: false,
                native_tool: if wl_copy { "wl-copy" } else { "arboard" },
                brand: TerminalName::Vte,
                display_server: crate::host::DisplayServer::Wayland,
                is_ssh: false,
                wayland_data_control: data_control,
                wl_copy_available: wl_copy,
                ..clipboard_input(TerminalName::Vte)
            });
            assert_eq!(diagnostics.has_issue, !expected.is_confirmed());
            assert!(diagnostics.text.contains(if data_control {
                "data-control on"
            } else {
                "data-control off"
            }));
        }
    }

    #[test]
    fn clipboard_diagnostics_tmux_wrap_and_container() {
        let tmux = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
            route_tmux: true,
            route_osc52: false,
            ..clipboard_input(TerminalName::Unknown)
        });
        assert!(tmux.text.contains("tmux         on"));
        assert!(tmux.text.contains("status       confirmed"));

        let wrapped = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
            osc52_sink: true,
            ..clipboard_input(TerminalName::Unknown)
        });
        assert!(wrapped.text.contains("osc 52       supported"));
        assert!(wrapped.text.contains("wrap         on"));
        assert!(wrapped.text.contains("status       confirmed"));

        let container = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
            is_ssh: false,
            container_no_display: true,
            ..clipboard_input(TerminalName::Unknown)
        });
        assert!(container.text.contains("native       container (arboard)"));
        assert!(container.text.contains("status       unverified"));
        assert!(
            container
                .text
                .contains("fix          grok wrap <command> or /minimal")
        );

        let remote_container = format_clipboard_diagnostics(ClipboardDiagnosticsInput {
            container_no_display: true,
            ..clipboard_input(TerminalName::Unknown)
        });
        assert!(
            remote_container
                .text
                .contains("native       container (arboard)")
        );
        assert!(
            remote_container
                .text
                .contains("fix          grok wrap <ssh command> or /minimal")
        );
    }

    #[test]
    fn clipboard_all_good_modern_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("on"), "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_all_good_external() {
        let w = diagnose_clipboard_from_values(Some("external"), true, Some("all"), "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_all_good_old_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_off_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("off"), true, Some("on"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].fix.as_deref(), Some("set -g set-clipboard on"));
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn clipboard_query_unavailable_does_not_warn() {
        // `None` means the query failed — not proof that the setting is
        // disabled.  No warning should be emitted.
        let w = diagnose_clipboard_from_values(None, true, Some("on"), "~/.tmux.conf");
        assert!(
            w.is_empty(),
            "Query-unavailable set-clipboard should not produce a warning"
        );
    }

    #[test]
    fn dcs_passthrough_off_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("off"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].fix.as_deref(), Some("set -g allow-passthrough on"));
    }

    #[test]
    fn dcs_passthrough_query_unavailable_does_not_warn() {
        // `allow_passthrough` query returned `None` — the probe could not
        // obtain a value, so we must not claim the setting is disabled.
        let w = diagnose_clipboard_from_values(Some("on"), true, None, "~/.tmux.conf");
        assert!(
            w.is_empty(),
            "Query-unavailable allow-passthrough should not produce a warning"
        );
    }

    #[test]
    fn dcs_passthrough_not_checked_on_old_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_both_bad_produces_two_warnings() {
        let w = diagnose_clipboard_from_values(Some("off"), true, Some("off"), "~/.tmux.conf");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    #[test]
    fn clipboard_both_bad_old_tmux_produces_one_warning() {
        let w = diagnose_clipboard_from_values(Some("off"), false, None, "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
    }

    #[test]
    fn clipboard_byobu_config_path_propagated() {
        let w =
            diagnose_clipboard_from_values(Some("off"), true, Some("on"), "~/.byobu/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // =====================================================================
    // diagnose_wayland_data_control: pure Wayland clipboard logic
    // =====================================================================

    #[test]
    fn wayland_no_data_control_warns() {
        let w = diagnose_wayland_data_control(true, false, true).expect("must warn");
        assert_eq!(w.category, WarningCategory::WaylandNoDataControl);
        assert!(w.message.contains("focused"));
        assert!(w.fix.is_none(), "wl-copy present: nothing to install");
    }

    #[test]
    fn wayland_no_data_control_missing_wl_copy_suggests_install() {
        let w = diagnose_wayland_data_control(true, false, false).expect("must warn");
        assert_eq!(w.category, WarningCategory::WaylandNoDataControl);
        assert!(
            w.fix.as_deref().is_some_and(|f| f.contains("wl-clipboard")),
            "fix must suggest installing wl-clipboard, got: {:?}",
            w.fix
        );
    }

    #[test]
    fn wayland_with_data_control_is_quiet() {
        assert!(diagnose_wayland_data_control(true, true, true).is_none());
        assert!(diagnose_wayland_data_control(true, true, false).is_none());
    }

    #[test]
    fn non_wayland_is_quiet() {
        assert!(diagnose_wayland_data_control(false, false, false).is_none());
        assert!(diagnose_wayland_data_control(false, true, true).is_none());
    }

    // =====================================================================
    // collect_startup_warnings: full integration
    // =====================================================================

    // -- Plain terminal: no warnings ------------------------------------------

    #[test]
    fn plain_terminal_no_warnings() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Plain terminal should produce no warnings");
    }

    // -- Healthy tmux: no warnings --------------------------------------------

    #[test]
    fn healthy_tmux_fullscreen_no_warnings() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Healthy tmux fullscreen should be quiet");
    }

    #[test]
    fn healthy_tmux_inline_no_warnings() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, false);
        assert!(w.is_empty(), "Healthy tmux inline should be quiet");
    }

    // -- tmux clipboard misconfiguration --------------------------------------

    #[test]
    fn tmux_clipboard_off_warns() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn tmux_dcs_passthrough_off_warns() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn tmux_both_clipboard_issues_warns_twice() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    // -- tmux control mode ----------------------------------------------------

    #[test]
    fn tmux_control_mode_inline_warns_degraded() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // control_mode=true, fullscreen_active=false → inline degraded message.
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert!(
            w[0].message.contains("degraded inline"),
            "Inline control-mode should mention degraded inline mode"
        );
    }

    #[test]
    fn tmux_control_mode_fullscreen_warns_unreliable() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // control_mode=true, fullscreen_active=true → fullscreen unreliable message.
        let w = collect_startup_warnings(&ctx, &query, true, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert!(
            w[0].message.contains("unreliable"),
            "Fullscreen control-mode should warn about unreliable fullscreen"
        );
        assert!(
            !w[0].message.contains("degraded inline"),
            "Fullscreen control-mode should NOT mention degraded inline mode"
        );
    }

    #[test]
    fn tmux_control_mode_with_clipboard_issue_shows_both() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::ControlMode));
        assert!(categories.contains(&WarningCategory::Clipboard));
    }

    // -- Byobu-on-tmux -------------------------------------------------------

    #[test]
    fn byobu_tmux_healthy_no_warnings() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Healthy Byobu-tmux should be quiet");
    }

    #[test]
    fn byobu_tmux_clipboard_off_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // -- Byobu-on-screen ------------------------------------------------------

    #[test]
    fn byobu_screen_warns_best_effort() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
        assert!(w[0].fix.is_none(), "Byobu-screen has no actionable fix");
    }

    #[test]
    fn byobu_screen_does_not_show_tmux_warnings() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        // Only the ByobuScreen warning, no clipboard/DCS warnings.
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
    }

    // -- Plain screen (no Byobu) ----------------------------------------------

    #[test]
    fn plain_screen_no_warnings() {
        let ctx = plain_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain screen should not inherit Byobu or tmux warnings"
        );
    }

    // -- Zellij ---------------------------------------------------------------

    #[test]
    fn zellij_no_warnings() {
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, false);
        assert!(
            w.is_empty(),
            "Zellij should not show tmux or Byobu warnings"
        );
    }

    // -- Apple Terminal (unsupported OSC 52) ----------------------------------

    #[test]
    fn apple_terminal_ssh_warns() {
        let query = FakeTmuxQuery::healthy_modern();
        let warnings = collect_startup_warnings(&apple_terminal_ctx(true), &query, false, true);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].category, WarningCategory::UnsupportedTerminal);
        assert!(warnings[0].fix.is_none());
    }

    #[test]
    fn apple_terminal_non_ssh_no_warning() {
        let query = FakeTmuxQuery::healthy_modern();
        let warnings = collect_startup_warnings(&apple_terminal_ctx(false), &query, false, true);
        assert!(warnings.is_empty());
    }

    // -- Multi-warning coalescing ---------------------------------------------

    #[test]
    fn tmux_clipboard_and_dcs_both_warn() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
    }

    #[test]
    fn tmux_control_mode_with_all_issues() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::ControlMode));
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
    }

    // -- Query unavailable: tmux server unreachable ---------------------------

    #[test]
    fn tmux_query_unavailable_produces_no_clipboard_warnings() {
        // When the tmux server is unreachable, all queries return `None`.
        // The diagnostics engine must not claim settings are disabled.
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Unavailable tmux queries should not produce warnings"
        );
    }

    #[test]
    fn tmux_query_unavailable_in_control_mode_only_shows_control_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
    }

    #[test]
    fn tmux_query_error_does_not_warn() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::error();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_query_unavailable_all_none_no_warnings() {
        // All probes return `None` → no warnings from clipboard diagnostics.
        let w = diagnose_clipboard_from_values(None, false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_query_unavailable_passthrough_exists_but_value_none() {
        // `option_exists` returned true but `show_option` returned `None`:
        // the server knows the option but couldn't read it.  No warning.
        let w = diagnose_clipboard_from_values(Some("on"), true, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    // =====================================================================
    // Extended diagnostic matrix (final hardening)
    // =====================================================================

    // -- Non-standard option values trigger warnings --------------------------

    #[test]
    fn clipboard_disabled_string_is_flagged() {
        // Some tmux configurations return "disabled" instead of "off".
        let w = diagnose_clipboard_from_values(Some("disabled"), true, Some("on"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
    }

    #[test]
    fn passthrough_disabled_string_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("disabled"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
    }

    // -- Zellij produces no tmux-specific warnings ----------------------------

    #[test]
    fn zellij_fullscreen_active_no_warnings() {
        // Zellij with fullscreen_active=true: no tmux warnings should appear.
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Zellij should not produce warnings even with fullscreen active"
        );
    }

    #[test]
    fn zellij_with_bad_tmux_options_still_quiet() {
        // Even if the fake tmux query reports bad options, Zellij context
        // should not produce tmux-specific warnings because it is not tmux-backed.
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Zellij should not inherit tmux option warnings"
        );
    }

    // -- Plain terminal with bad tmux options: no warnings --------------------

    #[test]
    fn plain_terminal_with_bad_tmux_options_still_quiet() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain terminal should not show tmux option warnings"
        );
    }

    // -- Plain screen with bad tmux options: no warnings ----------------------

    #[test]
    fn plain_screen_with_bad_tmux_options_no_warnings() {
        let ctx = plain_screen_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain screen should not produce tmux-specific warnings"
        );
    }

    // -- WezTerm without the Kitty keyboard protocol ---------------------------

    fn wezterm_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::WezTerm,
            ..Default::default()
        }
    }

    #[test]
    fn wezterm_no_kkp_warns_with_config_fix() {
        let w = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None)
            .expect("WezTerm without pushed KKP flags must warn");
        assert_eq!(w.category, WarningCategory::WezTermKittyKeyboardOff);
        assert_eq!(
            w.fix.as_deref(),
            Some("config.enable_kitty_keyboard = true")
        );
        assert_eq!(
            w.config_path.as_deref(),
            Some("~/.config/wezterm/wezterm.lua")
        );
        assert!(
            w.note.as_deref().is_some_and(|n| n.contains("\\")),
            "note must mention the backslash+Enter workaround"
        );
    }

    #[test]
    fn wezterm_with_kkp_active_no_warning() {
        // Probe passed and flags were pushed (e.g. enable_kitty_keyboard =
        // true already set) — Shift+Enter works, stay quiet.
        assert!(wezterm_kitty_keyboard_warning(&wezterm_ctx(), true, None).is_none());
    }

    #[test]
    fn non_wezterm_no_kkp_no_warning() {
        // Other brands without KKP are handled by their own paths (hint
        // substitution, brand skip lists) — this warning is WezTerm-only.
        let ctx = plain_terminal_ctx();
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, None).is_none());
    }

    #[test]
    fn wezterm_inside_tmux_with_skip_reason_no_warning() {
        // A kitty skip reason (old tmux) means KKP was never probed; the
        // wezterm.lua change alone wouldn't help, so don't advertise it.
        let ctx = TerminalContext {
            brand: TerminalName::WezTerm,
            multiplexer: MultiplexerKind::Tmux,
            ..Default::default()
        };
        assert!(ctx.kitty_skip_reason().is_some(), "fixture must skip KKP");
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, None).is_none());
    }

    #[test]
    fn wezterm_over_ssh_via_xtversion_warns() {
        // SSH shape: env brand Unknown (TERM_PROGRAM not forwarded), but
        // the terminal self-reported as WezTerm via XTVERSION. KKP is
        // skipped for Unknown brands, so flags were never pushed — warn.
        let ctx = TerminalContext {
            is_ssh: true,
            ..Default::default()
        };
        assert_eq!(ctx.brand, TerminalName::Unknown);
        let w = wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 20240203-110809"))
            .expect("XTVERSION-identified WezTerm over SSH must warn");
        assert_eq!(w.category, WarningCategory::WezTermKittyKeyboardOff);
        // The pager never negotiates KKP for Unknown brands, so the
        // wezterm.lua change cannot fix SSH sessions — the SSH variant
        // must NOT advertise it as the fix, and must lead with the
        // backslash+Enter workaround instead.
        assert!(
            w.fix.is_none(),
            "SSH variant must not advertise a config fix it can't honor"
        );
        assert!(w.message.contains("over SSH"));
        assert!(
            w.note
                .as_deref()
                .is_some_and(|n| n.starts_with("Type `\\`")),
            "SSH note must lead with the backslash+Enter workaround"
        );
    }

    #[test]
    fn wezterm_over_ssh_with_kkp_active_no_warning() {
        // Hypothetical future where KKP was negotiated despite the
        // Unknown brand — flags pushed wins over the XTVERSION report.
        let ctx = TerminalContext::default();
        assert!(wezterm_kitty_keyboard_warning(&ctx, true, Some("WezTerm 20240203")).is_none());
    }

    #[test]
    fn unknown_brand_non_wezterm_xtversion_no_warning() {
        // Self-report names a different terminal — stay quiet.
        let ctx = TerminalContext::default();
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("kitty 0.35.2")).is_none());
        // tmux answering XTVERSION must not be mistaken for a brand.
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("tmux 3.4")).is_none());
    }

    #[test]
    fn xtversion_wezterm_under_multiplexer_no_warning() {
        // Under tmux the XTVERSION reply describes tmux; even if a stale
        // "WezTerm" payload appeared, the multiplexer gate rejects it.
        let ctx = TerminalContext {
            multiplexer: MultiplexerKind::Tmux,
            ..Default::default()
        };
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 2024")).is_none());
    }

    #[test]
    fn xtversion_wezterm_local_not_ssh_no_warning() {
        // Local WezTerm with TERM_PROGRAM stripped (brand falls back to
        // Unknown) can still answer XTVERSION with "WezTerm". The
        // XTVERSION path is SSH-only, so without is_ssh we must NOT emit
        // the "over SSH" copy here -- that would be wrong (it's local) and
        // would drop the actionable wezterm.lua fix. Env-based detection
        // covers the actionable local case; stay quiet otherwise.
        let ctx = TerminalContext {
            is_ssh: false,
            ..Default::default()
        };
        assert_eq!(ctx.brand, TerminalName::Unknown);
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 20240203")).is_none());
    }

    // -- assemble_startup_warnings: banner ordering ----------------------------

    fn clipboard_banner() -> crate::startup::StartupWarning {
        crate::startup::StartupWarning {
            severity: crate::startup::WarningSeverity::Warning,
            message: "Clipboard may be unreachable.".to_string(),
            action: None,
        }
    }

    #[test]
    fn wezterm_banner_goes_first() {
        // The welcome screen renders only the first warning; the WezTerm
        // banner (broken local input) must displace clipboard advisories.
        let w = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None).unwrap();
        let out = assemble_startup_warnings(Some(&w), None, None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 2);
        assert!(
            out[0].message.contains("WezTerm"),
            "WezTerm banner must be first, got: {}",
            out[0].message
        );
        assert!(out[1].message.contains("Clipboard"));
    }

    #[test]
    fn no_wezterm_warning_leaves_summarized_untouched() {
        let out = assemble_startup_warnings(None, None, None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 1);
        assert!(out[0].message.contains("Clipboard"));
    }

    #[test]
    fn wayland_banner_surfaces_without_ssh_gate() {
        // `summarize_warnings` is SSH-gated, so the Wayland warning reaches the
        // welcome banner through the assemble bypass instead.
        let w = diagnose_wayland_data_control(true, false, true).unwrap();
        let out = assemble_startup_warnings(None, Some(&w), None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 2);
        assert!(
            out[0].message.contains("focused"),
            "Wayland banner must be first, got: {}",
            out[0].message
        );
        assert!(out[1].message.contains("Clipboard"));
    }

    #[test]
    fn wezterm_banner_outranks_wayland_banner() {
        let wez = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None).unwrap();
        let way = diagnose_wayland_data_control(true, false, true).unwrap();
        let out = assemble_startup_warnings(Some(&wez), Some(&way), None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 3);
        assert!(out[0].message.contains("WezTerm"));
        assert!(out[1].message.contains("focused"));
        assert!(out[2].message.contains("Clipboard"));
    }

    #[test]
    fn sandbox_profile_conflict_warning_reports_conflicts() {
        assert!(sandbox_profile_conflict_warning_from(vec![]).is_none());

        let w = sandbox_profile_conflict_warning_from(vec!["dev".to_string()]).unwrap();
        assert_eq!(w.category, WarningCategory::SandboxProfileConflict);
        assert!(
            w.message
                .starts_with("Your project sandbox profile conflicts with user config.")
        );
        assert!(w.message.contains("Profile: 'dev'"));
        assert_eq!(w.fix.as_deref(), Some("Using the user profile instead."));
    }

    #[test]
    fn sandbox_banner_sits_below_terminal_banners() {
        let sandbox = sandbox_profile_conflict_warning_from(vec!["dev".to_string()]).unwrap();

        let out = assemble_startup_warnings(None, None, Some(&sandbox), vec![]);
        assert_eq!(out.len(), 1);
        assert!(out[0].message.contains("sandbox profile"));

        let wez = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None).unwrap();
        let out = assemble_startup_warnings(Some(&wez), None, Some(&sandbox), vec![]);
        assert_eq!(out.len(), 2);
        assert!(out[0].message.contains("WezTerm"));
        assert!(out[1].message.contains("sandbox profile"));
    }

    // -- ssh_wrap_hint: `grok wrap ssh` recommendation --------------------------

    #[test]
    fn ssh_wrap_hint_fires_over_plain_ssh() {
        // is_ssh, no sink, not VS Code remote → recommend wrap.
        let w = ssh_wrap_hint(true, false, false).expect("hint must fire");
        assert_eq!(w.category, WarningCategory::SshWithoutWrap);
        assert_eq!(w.fix.as_deref(), Some("grok wrap ssh <host>"));
        assert!(
            w.config_path.is_none(),
            "fix is a command, not a config line"
        );
        assert!(
            w.note
                .as_deref()
                .is_some_and(|n| n.contains("local machine")),
            "note must say where to run the command, got: {:?}",
            w.note
        );
    }

    #[test]
    fn ssh_wrap_hint_suppressed_without_ssh() {
        assert!(ssh_wrap_hint(false, false, false).is_none());
    }

    #[test]
    fn ssh_wrap_hint_suppressed_when_sink_active() {
        // An active OSC 52 sink means the session already runs under
        // `grok wrap` — adoption silences the hint by itself.
        assert!(ssh_wrap_hint(true, true, false).is_none());
    }

    #[test]
    fn ssh_wrap_hint_suppressed_in_vscode_remote() {
        // VS Code remote's integrated terminal is not a plain ssh terminal
        // the user could wrap.
        assert!(ssh_wrap_hint(true, false, true).is_none());
    }

    // -- Warning ordering: clipboard before DCS --------------------------------

    #[test]
    fn warning_order_clipboard_then_dcs() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    #[test]
    fn control_mode_warning_comes_before_clipboard() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert_eq!(w[1].category, WarningCategory::Clipboard);
    }

    // -- Byobu-tmux DCS passthrough uses Byobu config path --------------------

    #[test]
    fn byobu_tmux_dcs_passthrough_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // -- Byobu-screen ignores control mode flag (no tmux to be in control mode)

    #[test]
    fn byobu_screen_ignores_control_mode_flag() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // Even with control_mode=true, Byobu-screen produces only ByobuScreen warning.
        let w = collect_startup_warnings(&ctx, &query, true, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
    }

    // -- Byobu-tmux with all issues: complete coalesced set -------------------

    #[test]
    fn byobu_tmux_all_issues_fullscreen() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
        // All should point to Byobu config.
        for warning in &w {
            assert_eq!(warning.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        }
    }

    // -- tmux extended-keys off warning ---------------------------------------

    fn extended_keys_ctx(base: TerminalContext, val: Option<&str>) -> TerminalContext {
        TerminalContext {
            tmux_version: Some("tmux 3.4".to_owned()),
            tmux_extended_keys: val.map(str::to_owned),
            ..base
        }
    }

    fn collect_extended_keys_warnings(ctx: &TerminalContext) -> Vec<TerminalWarning> {
        let query = FakeTmuxQuery::healthy_modern();
        collect_startup_warnings(ctx, &query, false, true)
            .into_iter()
            .filter(|w| w.category == WarningCategory::TmuxExtendedKeysOff)
            .collect()
    }

    fn assert_no_extended_keys_warning(val: Option<&str>) {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), val);
        assert!(collect_extended_keys_warnings(&ctx).is_empty());
    }

    #[test]
    fn tmux_extended_keys_off_emits_warning() {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let extended = warnings.first().expect("warning must fire");
        assert_eq!(
            extended.message,
            "tmux extended-keys is off -- modifier key combinations may not reach the pager"
        );
        assert_eq!(extended.fix.as_deref(), Some("set -g extended-keys on"));
        assert_eq!(extended.config_path.as_deref(), Some("~/.tmux.conf"));
        let note = extended.note.as_deref().expect("note must be present");
        assert!(note.contains("source-file") || note.contains("reattach"));
        assert!(note.contains("~/.tmux.conf"));
    }

    #[test]
    fn tmux_extended_keys_off_uses_byobu_config_path() {
        let ctx = extended_keys_ctx(byobu_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let extended = warnings.first().expect("warning must fire");
        assert_eq!(extended.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        let note = extended.note.as_deref().expect("note must be present");
        assert!(note.contains("~/.byobu/.tmux.conf"));
        // `~/.byobu/.tmux.conf` does not contain `~/.tmux.conf` as a
        // contiguous substring, so this catches the hardcoded-path
        // bug.
        assert!(!note.contains("~/.tmux.conf"));
    }

    #[test]
    fn tmux_extended_keys_no_warning_for_non_off_values() {
        assert_no_extended_keys_warning(None);
        assert_no_extended_keys_warning(Some("on"));
        assert_no_extended_keys_warning(Some("always"));
    }

    // -- summarize_warnings allow-list -----------------------------------------

    #[test]
    fn summarize_warnings_surfaces_extended_keys_off() {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let summary = summarize_warnings_inner(&warnings).expect("welcome banner must surface");
        assert_eq!(summary.severity, crate::startup::WarningSeverity::Warning);
        assert_eq!(summary.message, "Clipboard may be unreachable.");
        assert_eq!(
            summary.action.as_deref(),
            Some("See /terminal-setup for potential fixes.")
        );
    }

    #[test]
    fn summarize_warnings_surfaces_dcs_passthrough_off() {
        let warnings =
            diagnose_clipboard_from_values(Some("on"), true, Some("off"), "~/.tmux.conf");
        let summary = summarize_warnings_inner(&warnings).expect("welcome banner must surface");
        assert_eq!(summary.severity, crate::startup::WarningSeverity::Warning);
        assert_eq!(summary.message, "Clipboard may be unreachable.");
        assert_eq!(
            summary.action.as_deref(),
            Some("See /terminal-setup for potential fixes.")
        );
    }

    #[test]
    fn summarize_warnings_suppresses_other_categories() {
        // Clipboard warnings stay suppressed: the welcome-banner allow-list
        // is intentionally narrow until each category's false-positive rate
        // is characterized.
        let warnings = diagnose_clipboard_from_values(Some("off"), false, None, "~/.tmux.conf");
        assert!(
            !warnings.is_empty(),
            "fixture sanity: clipboard warnings must fire"
        );
        assert!(summarize_warnings_inner(&warnings).is_none());
    }

    #[test]
    fn summarize_warnings_picks_allowed_when_mixed_with_others() {
        // When clipboard (suppressed) AND DCS passthrough (allowed) both
        // fire, the banner still surfaces.
        let warnings =
            diagnose_clipboard_from_values(Some("off"), true, Some("off"), "~/.tmux.conf");
        assert!(
            warnings.len() >= 2,
            "fixture sanity: multiple warnings must fire"
        );
        let summary = summarize_warnings_inner(&warnings).expect("surfaces allowed warning");
        assert_eq!(summary.message, "Clipboard may be unreachable.");
    }

    #[test]
    fn summarize_warnings_empty_input_returns_none() {
        assert!(summarize_warnings_inner(&[]).is_none());
    }

    #[test]
    fn summarize_warnings_suppressed_when_not_ssh() {
        // Even with a valid allow-listed warning, the banner is suppressed
        // when not running over SSH — locally these misconfigurations don't
        // actually break clipboard.
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        assert!(
            !warnings.is_empty(),
            "fixture sanity: extended-keys warning must fire"
        );
        assert!(summarize_warnings(&warnings, false).is_none());
    }

    // =====================================================================
    // collect_notification_warnings
    // =====================================================================

    use crate::notifications::NotificationCondition;
    use crate::notifications::protocol::NotificationProtocol;

    #[test]
    fn notification_bel_fallback_for_unknown_terminal() {
        let ctx = plain_terminal_ctx();
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..ctx
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::NotificationProtocolFallback);
        assert!(w[0].message.contains("BEL"));
    }

    #[test]
    fn notification_bel_for_known_terminal_no_warning() {
        let ctx = TerminalContext {
            brand: TerminalName::Alacritty,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty(), "BEL on known terminal should not warn");
    }

    #[test]
    fn notification_osc_protocol_no_warning_without_tmux() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc99,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn notification_tmux_passthrough_off_warns_for_osc_protocol() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert!(w[0].message.contains("notification"));
        assert_eq!(w[0].fix.as_deref(), Some("set -g allow-passthrough on"));
    }

    #[test]
    fn notification_tmux_passthrough_on_no_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn notification_tmux_passthrough_bel_protocol_no_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty(), "BEL does not use DCS passthrough");
    }

    #[test]
    fn notification_focus_tracking_unavailable_unknown_terminal() {
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let focus_warnings: Vec<_> = w
            .iter()
            .filter(|w| w.category == WarningCategory::FocusTrackingUnavailable)
            .collect();
        assert_eq!(focus_warnings.len(), 1);
        assert!(focus_warnings[0].message.contains("focus tracking"));
        assert!(focus_warnings[0].fix.as_deref().unwrap().contains("always"));
    }

    #[test]
    fn notification_focus_tracking_unavailable_apple_terminal() {
        let ctx = apple_terminal_ctx(false);
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let focus_warnings: Vec<_> = w
            .iter()
            .filter(|w| w.category == WarningCategory::FocusTrackingUnavailable)
            .collect();
        assert_eq!(focus_warnings.len(), 1);
    }

    #[test]
    fn notification_focus_tracking_no_warning_when_condition_always() {
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(
            !w.iter()
                .any(|w| w.category == WarningCategory::FocusTrackingUnavailable),
            "condition=always does not need focus tracking"
        );
    }

    #[test]
    fn notification_focus_tracking_no_warning_for_supported_terminal() {
        let ctx = plain_terminal_ctx(); // Ghostty supports focus tracking
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc777,
            NotificationCondition::Unfocused,
            &query,
        );
        assert!(
            !w.iter()
                .any(|w| w.category == WarningCategory::FocusTrackingUnavailable),
            "Ghostty supports focus tracking"
        );
    }

    #[test]
    fn notification_multiple_warnings_can_coexist() {
        // Unknown terminal in tmux with passthrough off and unfocused condition
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Tmux,
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%0".to_owned()),
            },
            ..Default::default()
        };
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        // BEL on unknown + unfocused → fallback + focus tracking warnings
        // (BEL doesn't use passthrough, so no passthrough warning)
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::NotificationProtocolFallback));
        assert!(categories.contains(&WarningCategory::FocusTrackingUnavailable));
    }

    #[test]
    fn notification_tmux_passthrough_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    #[test]
    fn notification_query_unavailable_no_passthrough_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc99,
            NotificationCondition::Always,
            &query,
        );
        assert!(
            w.is_empty(),
            "Unavailable tmux queries should not produce notification warnings"
        );
    }

    #[test]
    fn notification_none_protocol_no_warnings() {
        let ctx = TerminalContext {
            brand: TerminalName::GrokDesktop,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::None,
            NotificationCondition::Unfocused,
            &query,
        );
        assert!(w.is_empty(), "None protocol should produce no warnings");
    }

    #[test]
    fn supports_focus_tracking_known_terminals() {
        assert!(supports_focus_tracking(TerminalName::Kitty));
        assert!(supports_focus_tracking(TerminalName::Ghostty));
        assert!(supports_focus_tracking(TerminalName::Iterm2));
        assert!(supports_focus_tracking(TerminalName::WezTerm));
        assert!(supports_focus_tracking(TerminalName::Alacritty));
        assert!(supports_focus_tracking(TerminalName::Vte));
        assert!(supports_focus_tracking(TerminalName::Terminator));
        assert!(supports_focus_tracking(TerminalName::WarpTerminal));
        assert!(supports_focus_tracking(TerminalName::VsCode));
        assert!(supports_focus_tracking(TerminalName::GrokDesktop));
        assert!(!supports_focus_tracking(TerminalName::AppleTerminal));
        assert!(!supports_focus_tracking(TerminalName::Unknown));
        assert!(!supports_focus_tracking(TerminalName::Otty));
    }

    // -- Color / theme rows + LimitedColorSupport warnings --------------------

    #[test]
    fn color_support_warning_none_on_truecolor() {
        assert!(
            color_support_warning(
                ColorLevel::TrueColor,
                TerminalName::Ghostty,
                false,
                "~/.tmux.conf"
            )
            .is_none()
        );
    }

    #[test]
    fn color_support_warning_no_color() {
        let w = color_support_warning(
            ColorLevel::None,
            TerminalName::Ghostty,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert!(w.message.contains("NO_COLOR"));
        assert!(w.note.as_deref().is_some_and(|n| n.contains("NO_COLOR")));
    }

    #[test]
    fn color_support_warning_apple_terminal() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::AppleTerminal,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert!(w.message.contains("Terminal.app"));
        assert!(w.fix.is_none());
        assert!(
            w.note
                .as_deref()
                .is_some_and(|n| n.contains("e.g. Ghostty"))
        );
    }

    #[test]
    fn color_support_warning_tmux() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::Unknown,
            true,
            "~/.byobu/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert_eq!(
            w.fix.as_deref(),
            Some("set -as terminal-features \",*:RGB\"")
        );
        assert_eq!(w.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        let note = w.note.as_deref().expect("note");
        assert!(note.contains("COLORTERM=truecolor") && note.contains("tmux-256color"));
    }

    #[test]
    fn color_support_warning_colorterm() {
        let w = color_support_warning(
            ColorLevel::Basic,
            TerminalName::Unknown,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.fix.as_deref(), Some("export COLORTERM=truecolor"));
        assert!(w.config_path.is_none());
    }

    #[test]
    fn summarize_warnings_suppresses_limited_color_support() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::Unknown,
            false,
            "~/.tmux.conf",
        )
        .expect("fixture");
        assert!(summarize_warnings_inner(&[w]).is_none());
    }
}
