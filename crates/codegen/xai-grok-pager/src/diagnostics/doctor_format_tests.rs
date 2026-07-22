//! In-TUI `/doctor` formatter tests.

use super::format_doctor;
use crate::clipboard::ClipboardRoute;
use crate::diagnostics::probes::{
    ClipboardProbeFacts, DoctorProbeSnapshot, ProbeSnapshot, TmuxProbeFacts, TmuxProbeResult,
    TuiProbeEvidence, WaylandProbeFacts,
};
use crate::diagnostics::{
    ClipboardFacts, ColorFacts, DataControlFact, DiagnosticFacts, DiagnosticReport,
    DiagnosticSnapshot, KeyboardFact, RuntimeFact, view,
};
use crate::host::HostOs;
use crate::terminal::{
    ByobuBackend, ModifierDelivery, ModifierFate, MultiplexerKind, TerminalContext, TerminalName,
};
use crate::theme::color_support::ColorLevel;

static LOCAL_ROUTE: ClipboardRoute = ClipboardRoute {
    native: true,
    tmux_buffer: false,
    osc52: false,
    osc52_tmux_passthrough: false,
};
static SSH_ROUTE: ClipboardRoute = ClipboardRoute {
    native: true,
    tmux_buffer: false,
    osc52: true,
    osc52_tmux_passthrough: false,
};
static TMUX_ROUTE: ClipboardRoute = ClipboardRoute {
    native: true,
    tmux_buffer: true,
    osc52: true,
    osc52_tmux_passthrough: true,
};

fn unavailable_tmux() -> TmuxProbeFacts {
    TmuxProbeFacts {
        version: TmuxProbeResult::Unavailable,
        extended_keys: TmuxProbeResult::Unavailable,
        set_clipboard: TmuxProbeResult::Unavailable,
        allow_passthrough_support: TmuxProbeResult::Unavailable,
        allow_passthrough: TmuxProbeResult::Unavailable,
        control_mode: TmuxProbeResult::Unavailable,
    }
}

fn snapshot<'a>(
    terminal: &'a TerminalContext,
    tmux: TmuxProbeFacts,
    route: &'static ClipboardRoute,
    native_tool: &'static str,
    osc52_sink_active: bool,
    color_level: ColorLevel,
    runtime: TuiProbeEvidence<'a>,
) -> DoctorProbeSnapshot<'a> {
    DoctorProbeSnapshot {
        common: ProbeSnapshot {
            terminal,
            tmux,
            wayland: WaylandProbeFacts {
                is_wayland: false,
                data_control: TmuxProbeResult::Available(false),
                wl_copy_available: false,
            },
            runtime,
        },
        clipboard: ClipboardProbeFacts {
            route: route.clone(),
            native_tool,
            osc52_sink_active,
        },
        host_os: HostOs::Macos,
        display_server: crate::host::DisplayServer::Unknown,
        container_no_display: false,
        color_level,
    }
}

fn runtime<'a>(xtversion: Option<&'a str>, kitty_flags_pushed: bool) -> TuiProbeEvidence<'a> {
    TuiProbeEvidence {
        fullscreen_active: true,
        kitty_flags_pushed,
        xtversion,
    }
}

fn ghostty(is_ssh: bool) -> TerminalContext {
    TerminalContext {
        brand: TerminalName::Ghostty,
        env_brand: TerminalName::Ghostty,
        is_ssh,
        ..Default::default()
    }
}

fn build_doctor(snapshot: DoctorProbeSnapshot<'_>) -> String {
    let report = view(DiagnosticSnapshot::from(snapshot));
    format_doctor(&report)
}

#[test]
fn healthy_local_output_is_stable() {
    let terminal = ghostty(false);
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &LOCAL_ROUTE,
        "pbcopy",
        false,
        ColorLevel::TrueColor,
        runtime(None, true),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     Ghostty\n",
            "  multiplexer  None detected\n",
            "  ssh          no\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       off\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
        )
    );
}

#[test]
fn tmux_config_and_reload_notes_output_is_stable() {
    let terminal = TerminalContext {
        brand: TerminalName::Iterm2,
        env_brand: TerminalName::Iterm2,
        multiplexer: MultiplexerKind::Tmux,
        byobu: Some(ByobuBackend::Tmux),
        tmux_version: Some("tmux 3.4".to_owned()),
        tmux_extended_keys: Some("off".to_owned()),
        ..Default::default()
    };
    let output = build_doctor(snapshot(
        &terminal,
        TmuxProbeFacts {
            version: TmuxProbeResult::Unavailable,
            extended_keys: TmuxProbeResult::Unavailable,
            set_clipboard: TmuxProbeResult::Available("off".to_owned()),
            allow_passthrough_support: TmuxProbeResult::Available(()),
            allow_passthrough: TmuxProbeResult::Available("off".to_owned()),
            control_mode: TmuxProbeResult::Available(false),
        },
        &TMUX_ROUTE,
        "pbcopy",
        false,
        ColorLevel::TrueColor,
        runtime(None, false),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     iTerm2\n",
            "  multiplexer  tmux\n",
            "  byobu        tmux\n",
            "  ssh          no\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         on\n",
            "  osc 52       supported\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "3 additional issue(s)\n",
            "\n",
            "  [!] OSC 52 clipboard passthrough is disabled\n",
            "      Fix: place `set -g set-clipboard on` in ~/.byobu/.tmux.conf\n",
            "\n",
            "  [!] DCS passthrough is disabled (needed for nested clipboard)\n",
            "      Fix: place `set -g allow-passthrough on` in ~/.byobu/.tmux.conf\n",
            "\n",
            "  [!] tmux extended-keys is off -- modifier key combinations may not reach the pager\n",
            "      Fix: place `set -g extended-keys on` in ~/.byobu/.tmux.conf\n",
            "      Note: Then reload tmux: `tmux source-file ~/.byobu/.tmux.conf` (or detach and reattach).\n",
        )
    );
}

#[test]
fn limited_color_output_is_stable() {
    let terminal = ghostty(false);
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &LOCAL_ROUTE,
        "pbcopy",
        false,
        ColorLevel::Ansi256,
        runtime(None, true),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     Ghostty\n",
            "  multiplexer  None detected\n",
            "  ssh          no\n",
            "  color        256\n",
            "  themes       2/5: groknight, grokday\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       off\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "1 additional issue(s)\n",
            "\n",
            "  [!] Color level is 256 -- truecolor themes unavailable\n",
            "      Fix: run `export COLORTERM=truecolor`\n",
            "      Note: Persist in ~/.zshrc / ~/.bashrc and restart Grok.\n",
        )
    );
}

#[test]
fn unwrapped_ssh_recommendation_with_no_issues_output_is_stable() {
    let terminal = ghostty(true);
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &SSH_ROUTE,
        "pbcopy",
        false,
        ColorLevel::TrueColor,
        runtime(None, true),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     Ghostty\n",
            "  multiplexer  None detected\n",
            "  ssh          yes\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       remote (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       supported\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
            "\n",
            "Recommendation\n",
            "\n",
            "  Running over SSH without `grok wrap` -- clipboard copies depend on the terminal's escape-sequence support, and a dropped connection can leave your local terminal in a bad state\n",
            "      Automatic setup: `grok doctor fix ssh-wrap`\n",
            "      One-off: `grok wrap ssh <host>`\n",
            "      Note: Run it on your local machine in place of plain `ssh` -- it forwards clipboard copies to your local system and restores terminal modes if the connection drops.\n",
        )
    );
}

#[test]
fn wrapped_ssh_output_has_no_recommendation() {
    let terminal = ghostty(true);
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &SSH_ROUTE,
        "pbcopy",
        true,
        ColorLevel::TrueColor,
        runtime(None, true),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     Ghostty\n",
            "  multiplexer  None detected\n",
            "  ssh          yes\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       remote (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       supported\n",
            "  wrap         on\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
        )
    );
}

#[test]
fn wezterm_xtversion_runtime_evidence_output_is_stable() {
    let terminal = TerminalContext {
        is_ssh: true,
        ..Default::default()
    };
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &SSH_ROUTE,
        "pbcopy",
        true,
        ColorLevel::TrueColor,
        runtime(Some("WezTerm 20240203-110809"), false),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     Unknown\n",
            "  xtversion    WezTerm 20240203-110809\n",
            "  multiplexer  None detected\n",
            "  ssh          yes\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       remote (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       supported\n",
            "  wrap         on\n",
            "  status       confirmed\n",
            "\n",
            "1 additional issue(s)\n",
            "\n",
            "  [!] WezTerm over SSH: Shift+Enter can't insert newlines\n",
            "      Note: Type `\\` then Enter to insert a newline. The pager doesn't negotiate the kitty keyboard protocol over SSH yet; `enable_kitty_keyboard = true` in wezterm.lua fixes local WezTerm sessions only.\n",
        )
    );
}

#[test]
fn unavailable_and_error_probes_do_not_create_false_issues() {
    let terminal = TerminalContext {
        brand: TerminalName::Iterm2,
        env_brand: TerminalName::Iterm2,
        multiplexer: MultiplexerKind::Tmux,
        tmux_version: Some("tmux 3.4".to_owned()),
        ..Default::default()
    };
    let output = build_doctor(snapshot(
        &terminal,
        TmuxProbeFacts {
            version: TmuxProbeResult::Unavailable,
            extended_keys: TmuxProbeResult::Unavailable,
            set_clipboard: TmuxProbeResult::Error("tmux server unreachable".to_owned()),
            allow_passthrough_support: TmuxProbeResult::Unavailable,
            allow_passthrough: TmuxProbeResult::Error("query failed".to_owned()),
            control_mode: TmuxProbeResult::Unavailable,
        },
        &TMUX_ROUTE,
        "pbcopy",
        false,
        ColorLevel::TrueColor,
        runtime(None, false),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     iTerm2\n",
            "  multiplexer  tmux\n",
            "  ssh          no\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         on\n",
            "  osc 52       supported\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
        )
    );
}

#[test]
fn vscode_newline_output_is_platform_neutral() {
    let terminal = TerminalContext {
        brand: TerminalName::VsCode,
        env_brand: TerminalName::VsCode,
        ..Default::default()
    };
    let output = build_doctor(snapshot(
        &terminal,
        unavailable_tmux(),
        &LOCAL_ROUTE,
        "pbcopy",
        false,
        ColorLevel::TrueColor,
        runtime(None, false),
    ));

    assert_eq!(
        output,
        concat!(
            "Environment\n",
            "  terminal     VS Code\n",
            "  multiplexer  None detected\n",
            "  ssh          no\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "  newline      Alt+Enter (VS Code: xterm.js can't distinguish Shift+Enter)\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       off\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
        )
    );
}

#[test]
fn keyboard_fact_formats_from_explicit_target_evidence() {
    let report = DiagnosticReport {
        facts: DiagnosticFacts {
            terminal: TerminalName::WezTerm,
            xtversion: RuntimeFact::NoReply,
            multiplexer: MultiplexerKind::Undetected,
            byobu: None,
            ssh: false,
            color: ColorFacts {
                level: RuntimeFact::Available(ColorLevel::TrueColor),
                available_themes: crate::theme::ThemeKind::ALL.to_vec(),
                total_themes: crate::theme::ThemeKind::ALL.len(),
            },
            keyboard: Some(KeyboardFact {
                modifier_delivery: ModifierDelivery::new_for_test(
                    ModifierFate::Dropped,
                    ModifierFate::Native,
                ),
                os: HostOs::Macos,
            }),
            newline: None,
            clipboard: ClipboardFacts {
                native_route: true,
                native_tool: "pbcopy".to_owned(),
                native_preflight: crate::clipboard::NativeClipboardPreflight::LocalAvailable,
                tmux_route: false,
                osc52_route: false,
                osc52_capability: crate::clipboard::Osc52Capability::Supported,
                wrap_sink: false,
                display_server: crate::host::DisplayServer::Unknown,
                container_no_display: false,
                data_control: DataControlFact::NotApplicable,
                delivery: crate::clipboard::ClipboardDelivery::Confirmed,
                fix: None,
            },
            voice: None,
        },
        findings: Vec::new(),
        probe_notes: Vec::new(),
    };

    assert_eq!(
        format_doctor(&report),
        concat!(
            "Environment\n",
            "  terminal     WezTerm\n",
            "  multiplexer  None detected\n",
            "  ssh          no\n",
            "  color        truecolor\n",
            "  themes       all\n",
            "  keyboard     cmd=dropped, opt=native (OS rescue active)\n",
            "\n",
            "Clipboard\n",
            "  native       local (pbcopy)\n",
            "  tmux         off\n",
            "  osc 52       off\n",
            "  wrap         off\n",
            "  status       confirmed\n",
            "\n",
            "No issues found.\n",
        )
    );
}
