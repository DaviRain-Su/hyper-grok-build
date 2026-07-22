//! Diagnostics view tests.

use super::*;
use crate::clipboard::ClipboardRoute;
use crate::diagnostics::probes::{
    ClipboardProbeFacts, TmuxProbeFacts, TuiProbeEvidence, WaylandProbeFacts,
};
use crate::terminal::{MultiplexerKind, TerminalContext, TerminalName};
use crate::theme::color_support::ColorLevel;

static ROUTE: ClipboardRoute = ClipboardRoute {
    native: true,
    tmux_buffer: true,
    osc52: true,
    osc52_tmux_passthrough: true,
};

fn snapshot<'a>(
    terminal: &'a TerminalContext,
    tmux: TmuxProbeFacts,
    runtime: DiagnosticRuntimeEvidence<'a>,
    osc52_sink_active: bool,
) -> DiagnosticSnapshot<'a> {
    snapshot_for_host(
        terminal,
        tmux,
        runtime,
        osc52_sink_active,
        crate::host::HostOs::Macos,
    )
}

fn snapshot_for_host<'a>(
    terminal: &'a TerminalContext,
    tmux: TmuxProbeFacts,
    runtime: DiagnosticRuntimeEvidence<'a>,
    osc52_sink_active: bool,
    host_os: crate::host::HostOs,
) -> DiagnosticSnapshot<'a> {
    snapshot_with_wayland_for_host(
        terminal,
        tmux,
        runtime,
        osc52_sink_active,
        WaylandProbeFacts {
            is_wayland: false,
            data_control: TmuxProbeResult::Unavailable,
            wl_copy_available: false,
        },
        host_os,
    )
}

fn snapshot_with_wayland<'a>(
    terminal: &'a TerminalContext,
    tmux: TmuxProbeFacts,
    runtime: DiagnosticRuntimeEvidence<'a>,
    osc52_sink_active: bool,
    wayland: WaylandProbeFacts,
) -> DiagnosticSnapshot<'a> {
    snapshot_with_wayland_for_host(
        terminal,
        tmux,
        runtime,
        osc52_sink_active,
        wayland,
        crate::host::HostOs::Macos,
    )
}

fn snapshot_with_wayland_for_host<'a>(
    terminal: &'a TerminalContext,
    tmux: TmuxProbeFacts,
    runtime: DiagnosticRuntimeEvidence<'a>,
    osc52_sink_active: bool,
    wayland: WaylandProbeFacts,
    host_os: crate::host::HostOs,
) -> DiagnosticSnapshot<'a> {
    DiagnosticSnapshot::from_parts(
        ProbeSnapshot {
            terminal,
            tmux,
            wayland,
            runtime: TuiProbeEvidence {
                fullscreen_active: false,
                kitty_flags_pushed: false,
                xtversion: match runtime.xtversion {
                    RuntimeEvidence::Available(xtversion) => xtversion,
                    RuntimeEvidence::Unavailable => None,
                },
            },
        },
        ClipboardProbeFacts {
            route: ROUTE.clone(),
            native_tool: "pbcopy",
            osc52_sink_active,
        },
        host_os,
        crate::host::DisplayServer::Unknown,
        false,
        ColorLevel::TrueColor,
        runtime,
    )
}

fn runtime(
    kitty_flags_pushed: RuntimeEvidence<bool>,
    xtversion: RuntimeEvidence<Option<&'static str>>,
) -> DiagnosticRuntimeEvidence<'static> {
    DiagnosticRuntimeEvidence {
        fullscreen_active: RuntimeEvidence::Available(true),
        kitty_flags_pushed,
        xtversion,
    }
}

fn available_runtime() -> DiagnosticRuntimeEvidence<'static> {
    runtime(
        RuntimeEvidence::Available(false),
        RuntimeEvidence::Available(None),
    )
}

#[test]
fn terminal_finding_ids_are_stable() {
    let ids = [
        WarningCategory::Clipboard,
        WarningCategory::DcsPassthrough,
        WarningCategory::ControlMode,
        WarningCategory::ByobuScreen,
        WarningCategory::UnsupportedTerminal,
        WarningCategory::TmuxExtendedKeysOff,
        WarningCategory::WaylandNoDataControl,
        WarningCategory::WezTermKittyKeyboardOff,
        WarningCategory::LimitedColorSupport,
        WarningCategory::SshWithoutWrap,
    ]
    .map(|category| id_for(category).expect("terminal setup category must have an ID"))
    .map(|id| id.to_string());

    assert_eq!(
        ids,
        [
            "terminal.tmux-clipboard",
            "terminal.dcs-passthrough",
            "terminal.control-mode",
            "terminal.byobu-screen",
            "terminal.unsupported-emulator",
            "terminal.tmux-extended-keys",
            "terminal.wayland-data-control",
            "terminal.wezterm-kitty",
            "terminal.limited-color",
            "terminal.ssh-wrap",
        ]
    );
}

#[test]
fn findings_have_stable_semantic_ids_and_dispositions() {
    let terminal = TerminalContext {
        brand: TerminalName::Iterm2,
        env_brand: TerminalName::Iterm2,
        multiplexer: MultiplexerKind::Tmux,
        is_ssh: true,
        ..Default::default()
    };
    let report = view(snapshot(
        &terminal,
        TmuxProbeFacts {
            version: TmuxProbeResult::Unavailable,
            extended_keys: TmuxProbeResult::Unavailable,
            set_clipboard: TmuxProbeResult::Available("off".to_owned()),
            allow_passthrough_support: TmuxProbeResult::Available(()),
            allow_passthrough: TmuxProbeResult::Available("on".to_owned()),
            control_mode: TmuxProbeResult::Available(false),
        },
        available_runtime(),
        false,
    ));

    assert_eq!(report.findings.len(), 2);
    assert_eq!(
        report.facts.clipboard.delivery,
        crate::clipboard::ClipboardDelivery::Confirmed
    );
    assert_eq!(
        report.facts.clipboard.native_preflight,
        crate::clipboard::NativeClipboardPreflight::RemoteOnly
    );
    assert_eq!(
        report.findings[0].id,
        DiagnosticId::new("terminal", "tmux-clipboard")
    );
    assert_eq!(report.findings[0].disposition, FindingDisposition::Issue);
    assert_eq!(
        report.findings[1].id,
        DiagnosticId::new("terminal", "ssh-wrap")
    );
    assert_eq!(
        report.findings[1].disposition,
        FindingDisposition::Recommendation
    );
    assert_eq!(
        report.findings[1].automatic_remediation,
        Some(crate::diagnostics::ssh_wrap_automatic_remediation())
    );
    assert!(report.findings[0].automatic_remediation.is_none());
}

#[test]
fn unavailable_runtime_evidence_is_honest_and_fail_open() {
    let terminal = TerminalContext {
        brand: TerminalName::WezTerm,
        env_brand: TerminalName::WezTerm,
        multiplexer: MultiplexerKind::Tmux,
        ..Default::default()
    };
    let report = view(snapshot(
        &terminal,
        TmuxProbeFacts {
            version: TmuxProbeResult::Unavailable,
            extended_keys: TmuxProbeResult::Unavailable,
            set_clipboard: TmuxProbeResult::Available("on".to_owned()),
            allow_passthrough_support: TmuxProbeResult::Available(()),
            allow_passthrough: TmuxProbeResult::Available("on".to_owned()),
            control_mode: TmuxProbeResult::Available(true),
        },
        DiagnosticRuntimeEvidence {
            fullscreen_active: RuntimeEvidence::Unavailable,
            kitty_flags_pushed: RuntimeEvidence::Unavailable,
            xtversion: RuntimeEvidence::Unavailable,
        },
        true,
    ));

    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.id == DiagnosticId::new("terminal", "wezterm-kitty"))
    );
    let control_mode = report
        .findings
        .iter()
        .find(|finding| finding.id == DiagnosticId::new("terminal", "control-mode"))
        .expect("control-mode finding");
    assert_eq!(
        control_mode.message,
        "tmux control mode detected -- terminal display may be degraded"
    );
    assert_eq!(
        report
            .probe_notes
            .iter()
            .filter(|note| note.probe.starts_with("runtime."))
            .count(),
        3
    );
}

#[test]
fn unavailable_and_error_probe_evidence_is_retained_without_findings() {
    let terminal = TerminalContext {
        brand: TerminalName::Iterm2,
        env_brand: TerminalName::Iterm2,
        multiplexer: MultiplexerKind::Tmux,
        ..Default::default()
    };
    let report = view(snapshot_with_wayland(
        &terminal,
        TmuxProbeFacts {
            version: TmuxProbeResult::Unavailable,
            extended_keys: TmuxProbeResult::Unavailable,
            set_clipboard: TmuxProbeResult::Error("server unreachable".to_owned()),
            allow_passthrough_support: TmuxProbeResult::Unsupported,
            allow_passthrough: TmuxProbeResult::Unavailable,
            control_mode: TmuxProbeResult::Unavailable,
        },
        available_runtime(),
        true,
        WaylandProbeFacts {
            is_wayland: true,
            data_control: TmuxProbeResult::Unavailable,
            wl_copy_available: false,
        },
    ));

    assert!(report.findings.is_empty());
    assert_eq!(
        report.facts.clipboard.delivery,
        crate::clipboard::ClipboardDelivery::Confirmed
    );
    assert_eq!(report.probe_notes.len(), 6);
    assert_eq!(report.probe_notes[0].probe, "tmux.version");
    assert_eq!(report.probe_notes[1].probe, "tmux.extended-keys");
    assert_eq!(report.probe_notes[2].status, ProbeStatus::Error);
    assert_eq!(
        report.probe_notes[2].message.as_deref(),
        Some("server unreachable")
    );
    assert_eq!(report.probe_notes[3].status, ProbeStatus::Unsupported);
    assert_eq!(report.probe_notes[4].probe, "tmux.control-mode");
    assert_eq!(report.probe_notes[5].probe, "wayland.data-control");
}

fn plain_tmux() -> TmuxProbeFacts {
    TmuxProbeFacts {
        version: TmuxProbeResult::Unavailable,
        extended_keys: TmuxProbeResult::Unavailable,
        set_clipboard: TmuxProbeResult::Available("on".to_owned()),
        allow_passthrough_support: TmuxProbeResult::Available(()),
        allow_passthrough: TmuxProbeResult::Available("on".to_owned()),
        control_mode: TmuxProbeResult::Available(false),
    }
}

#[test]
fn local_wezterm_without_kitty_evidence_has_no_alt_enter_fallback() {
    let terminal = TerminalContext {
        brand: TerminalName::WezTerm,
        env_brand: TerminalName::WezTerm,
        ..Default::default()
    };
    let report = view(snapshot(
        &terminal,
        plain_tmux(),
        runtime(
            RuntimeEvidence::Unavailable,
            RuntimeEvidence::Available(None),
        ),
        true,
    ));

    assert!(report.facts.newline.is_none());
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.id == DiagnosticId::new("terminal", "wezterm-kitty"))
    );
}

#[test]
fn ssh_xtversion_wezterm_without_kitty_evidence_has_no_alt_enter_fallback() {
    let terminal = TerminalContext {
        is_ssh: true,
        ..Default::default()
    };
    let report = view(snapshot(
        &terminal,
        plain_tmux(),
        runtime(
            RuntimeEvidence::Unavailable,
            RuntimeEvidence::Available(Some("WezTerm 20240203")),
        ),
        true,
    ));

    assert!(report.facts.newline.is_none());
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.id == DiagnosticId::new("terminal", "wezterm-kitty"))
    );
}

#[test]
fn non_wezterm_without_kitty_evidence_keeps_ordinary_fallback() {
    let terminal = TerminalContext {
        brand: TerminalName::VsCode,
        env_brand: TerminalName::VsCode,
        ..Default::default()
    };
    let report = view(snapshot(
        &terminal,
        plain_tmux(),
        runtime(
            RuntimeEvidence::Unavailable,
            RuntimeEvidence::Available(None),
        ),
        true,
    ));

    assert_eq!(
        report.facts.newline,
        Some(NewlineFact::XtermJs {
            terminal: TerminalName::VsCode,
        })
    );
}

#[test]
fn available_wezterm_evidence_retains_finding_and_backslash_note() {
    let terminal = TerminalContext {
        brand: TerminalName::WezTerm,
        env_brand: TerminalName::WezTerm,
        ..Default::default()
    };
    let report = view(snapshot(&terminal, plain_tmux(), available_runtime(), true));

    assert!(report.facts.newline.is_none());
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.id == DiagnosticId::new("terminal", "wezterm-kitty"))
        .expect("WezTerm finding");
    assert!(
        finding
            .note
            .as_deref()
            .is_some_and(|note| note.contains("type `\\` then Enter"))
    );
}

#[test]
fn keyboard_fact_and_formatter_use_snapshot_host() {
    let terminal = TerminalContext {
        brand: TerminalName::WezTerm,
        env_brand: TerminalName::WezTerm,
        ..Default::default()
    };
    let snapshot_host = if crate::host::HostOs::current() == crate::host::HostOs::Macos {
        crate::host::HostOs::Linux
    } else {
        crate::host::HostOs::Macos
    };
    assert_ne!(snapshot_host, crate::host::HostOs::current());
    let report = view(snapshot_for_host(
        &terminal,
        plain_tmux(),
        runtime(
            RuntimeEvidence::Available(true),
            RuntimeEvidence::Available(None),
        ),
        true,
        snapshot_host,
    ));

    let keyboard = report.facts.keyboard.as_ref();
    let output = crate::diagnostics::format_doctor(&report);
    if snapshot_host == crate::host::HostOs::Macos {
        assert_eq!(keyboard.map(|fact| fact.os), Some(snapshot_host));
        assert!(output.contains("(OS rescue active)"));
    } else {
        assert!(keyboard.is_none());
        assert!(!output.contains("  keyboard     "));
    }
}
