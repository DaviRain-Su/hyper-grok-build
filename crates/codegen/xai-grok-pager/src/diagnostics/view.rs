//! Interpretation of terminal probe snapshots.

use crate::diagnostics::probes::{
    CommonProbeSnapshot, DiagnosticRuntimeEvidence, DoctorProbeSnapshot, ProbeSnapshot,
    RuntimeEvidence, StandaloneDiagnosticSnapshot, TmuxProbeResult,
};
use crate::diagnostics::{
    ClipboardFacts, ColorFacts, DataControlFact, DiagnosticFacts, DiagnosticFinding, DiagnosticId,
    DiagnosticReport, FindingDisposition, KeyboardFact, ManualRemediation, NewlineFact, ProbeNote,
    ProbeStatus, RuntimeFact, TerminalWarning, WarningCategory,
};
use crate::terminal::TerminalName;

pub struct DiagnosticSnapshot<'a> {
    pub common: CommonProbeSnapshot<'a>,
    pub clipboard: crate::diagnostics::probes::ClipboardProbeFacts,
    pub host_os: crate::host::HostOs,
    pub display_server: crate::host::DisplayServer,
    pub container_no_display: bool,
    pub color_level: RuntimeEvidence<crate::theme::color_support::ColorLevel>,
    pub runtime: DiagnosticRuntimeEvidence<'a>,
}

impl<'a> DiagnosticSnapshot<'a> {
    pub fn from_parts(
        common: ProbeSnapshot<'a>,
        clipboard: crate::diagnostics::probes::ClipboardProbeFacts,
        host_os: crate::host::HostOs,
        display_server: crate::host::DisplayServer,
        container_no_display: bool,
        color_level: crate::theme::color_support::ColorLevel,
        runtime: DiagnosticRuntimeEvidence<'a>,
    ) -> Self {
        Self {
            common: CommonProbeSnapshot {
                terminal: common.terminal,
                tmux: common.tmux,
                wayland: common.wayland,
            },
            clipboard,
            host_os,
            display_server,
            container_no_display,
            color_level: RuntimeEvidence::Available(color_level),
            runtime,
        }
    }
}

impl<'a> From<DoctorProbeSnapshot<'a>> for DiagnosticSnapshot<'a> {
    fn from(snapshot: DoctorProbeSnapshot<'a>) -> Self {
        let runtime = snapshot.common.runtime.into();
        Self {
            common: CommonProbeSnapshot {
                terminal: snapshot.common.terminal,
                tmux: snapshot.common.tmux,
                wayland: snapshot.common.wayland,
            },
            clipboard: snapshot.clipboard,
            host_os: snapshot.host_os,
            display_server: snapshot.display_server,
            container_no_display: snapshot.container_no_display,
            color_level: RuntimeEvidence::Available(snapshot.color_level),
            runtime,
        }
    }
}

impl<'a> From<StandaloneDiagnosticSnapshot<'a>> for DiagnosticSnapshot<'a> {
    fn from(snapshot: StandaloneDiagnosticSnapshot<'a>) -> Self {
        Self {
            common: snapshot.common,
            clipboard: snapshot.clipboard,
            host_os: snapshot.host_os,
            display_server: snapshot.display_server,
            container_no_display: snapshot.container_no_display,
            color_level: snapshot.color_level,
            runtime: DiagnosticRuntimeEvidence {
                fullscreen_active: RuntimeEvidence::Unavailable,
                kitty_flags_pushed: RuntimeEvidence::Unavailable,
                xtversion: RuntimeEvidence::Unavailable,
            },
        }
    }
}

pub fn view(snapshot: DiagnosticSnapshot<'_>) -> DiagnosticReport {
    let ctx = snapshot.common.terminal;
    let wezterm_warning = wezterm_warning(&snapshot);
    let suppress_newline = wezterm_warning.is_some()
        || matches!(
            snapshot.runtime.kitty_flags_pushed,
            RuntimeEvidence::Unavailable
        ) && super::wezterm_shape(
            snapshot.common.terminal,
            runtime_xtversion(snapshot.runtime.xtversion),
        )
        .is_some();

    let mut warnings = startup_warnings(&snapshot);
    warnings.extend(super::diagnose_wayland_data_control_from_common(
        &snapshot.common,
    ));
    warnings.extend(wezterm_warning);
    if let RuntimeEvidence::Available(color_level) = snapshot.color_level {
        warnings.extend(super::color_support_warning(
            color_level,
            ctx.brand,
            ctx.is_tmux_backed(),
            &ctx.tmux_config_path(),
        ));
    }

    let mut findings = warnings.into_iter().filter_map(issue).collect::<Vec<_>>();
    findings.extend(
        super::ssh_wrap_hint(
            ctx.is_ssh,
            snapshot.clipboard.osc52_sink_active,
            ctx.is_official_vscode_remote,
        )
        .and_then(recommendation),
    );

    DiagnosticReport {
        facts: facts(&snapshot, suppress_newline),
        findings,
        probe_notes: probe_notes(&snapshot),
    }
}

fn startup_warnings(snapshot: &DiagnosticSnapshot<'_>) -> Vec<TerminalWarning> {
    let fullscreen_active = match snapshot.runtime.fullscreen_active {
        RuntimeEvidence::Available(fullscreen_active) => Some(fullscreen_active),
        RuntimeEvidence::Unavailable => None,
    };
    super::collect_startup_warnings_from(
        snapshot.common.terminal,
        &snapshot.common.tmux,
        fullscreen_active,
    )
}

fn wezterm_warning(snapshot: &DiagnosticSnapshot<'_>) -> Option<TerminalWarning> {
    let RuntimeEvidence::Available(kitty_flags_pushed) = snapshot.runtime.kitty_flags_pushed else {
        return None;
    };
    super::wezterm_kitty_keyboard_warning_from(
        snapshot.common.terminal,
        kitty_flags_pushed,
        runtime_xtversion(snapshot.runtime.xtversion),
    )
}

fn runtime_xtversion(evidence: RuntimeEvidence<Option<&str>>) -> Option<&str> {
    match evidence {
        RuntimeEvidence::Available(xtversion) => xtversion,
        RuntimeEvidence::Unavailable => None,
    }
}

fn facts(snapshot: &DiagnosticSnapshot<'_>, suppress_newline: bool) -> DiagnosticFacts {
    let ctx = snapshot.common.terminal;
    let available_themes = match snapshot.color_level {
        RuntimeEvidence::Available(color_level) => crate::theme::ThemeKind::ALL
            .iter()
            .copied()
            .filter(|kind| color_level.has_truecolor() || !kind.requires_truecolor())
            .collect(),
        RuntimeEvidence::Unavailable => Vec::new(),
    };
    let keyboard_capabilities =
        crate::terminal::keyboard_capabilities_for_host(ctx.brand, snapshot.host_os);
    let keyboard = (keyboard_capabilities
        .modifier_delivery
        .benefits_from_rescue()
        || keyboard_capabilities.enter_needs_rescue())
    .then_some(KeyboardFact {
        modifier_delivery: keyboard_capabilities.modifier_delivery,
        os: snapshot.host_os,
    });
    let newline = (ctx.shift_enter_unavailable() && !suppress_newline).then(|| {
        if ctx.vte_version.is_some() || ctx.brand == TerminalName::Vte {
            NewlineFact::Vte {
                version: ctx.vte_version.clone(),
            }
        } else if ctx.brand.is_vscode_family() {
            NewlineFact::XtermJs {
                terminal: ctx.brand,
            }
        } else {
            NewlineFact::NoKittyKeyboardProtocol
        }
    });
    let data_control = if !snapshot.common.wayland.is_wayland {
        DataControlFact::NotApplicable
    } else {
        match &snapshot.common.wayland.data_control {
            TmuxProbeResult::Available(true) => DataControlFact::Available,
            TmuxProbeResult::Available(false) => DataControlFact::Missing,
            TmuxProbeResult::Unavailable | TmuxProbeResult::Unsupported => {
                DataControlFact::Unavailable
            }
            TmuxProbeResult::Error(_) => DataControlFact::Error,
        }
    };
    let clipboard = clipboard_facts(snapshot, data_control);

    DiagnosticFacts {
        terminal: ctx.brand,
        xtversion: match snapshot.runtime.xtversion {
            RuntimeEvidence::Available(Some(xtversion)) => {
                RuntimeFact::Available(xtversion.to_owned())
            }
            RuntimeEvidence::Available(None) => RuntimeFact::NoReply,
            RuntimeEvidence::Unavailable => RuntimeFact::Unavailable,
        },
        multiplexer: ctx.multiplexer,
        byobu: ctx.byobu,
        ssh: ctx.is_ssh,
        color: ColorFacts {
            level: match snapshot.color_level {
                RuntimeEvidence::Available(level) => RuntimeFact::Available(level),
                RuntimeEvidence::Unavailable => RuntimeFact::Unavailable,
            },
            available_themes,
            total_themes: crate::theme::ThemeKind::ALL.len(),
        },
        keyboard,
        newline,
        clipboard,
        voice: None,
    }
}

fn clipboard_facts(
    snapshot: &DiagnosticSnapshot<'_>,
    data_control: DataControlFact,
) -> ClipboardFacts {
    use crate::clipboard::{ClipboardDelivery, ClipboardEnvironment, expected_delivery};

    let route = &snapshot.clipboard.route;
    let environment = ClipboardEnvironment {
        brand: snapshot.common.terminal.brand,
        host_os: snapshot.host_os,
        display_server: snapshot.display_server,
        remote: snapshot.common.terminal.is_ssh,
        container: snapshot.container_no_display,
        osc52_sink: snapshot.clipboard.osc52_sink_active,
        wayland_data_control: matches!(
            snapshot.common.wayland.data_control,
            TmuxProbeResult::Available(true)
        ),
        wl_copy_available: snapshot.common.wayland.wl_copy_available,
    };
    let native_preflight = crate::clipboard::native_clipboard_preflight(route.native, environment);
    let delivery = expected_delivery(
        native_preflight,
        route.tmux_buffer,
        route.osc52,
        environment,
    );
    let fix = match delivery {
        ClipboardDelivery::Confirmed => None,
        ClipboardDelivery::Unverified | ClipboardDelivery::Failed
            if snapshot.common.terminal.is_ssh =>
        {
            Some("grok wrap <ssh command> or /minimal")
        }
        ClipboardDelivery::Unverified | ClipboardDelivery::Failed
            if snapshot.container_no_display =>
        {
            Some("grok wrap <command> or /minimal")
        }
        ClipboardDelivery::Unverified => Some("grok wrap or /minimal"),
        ClipboardDelivery::Failed => Some("/minimal"),
    };

    ClipboardFacts {
        native_route: route.native,
        native_tool: snapshot.clipboard.native_tool.to_owned(),
        native_preflight,
        tmux_route: route.tmux_buffer,
        osc52_route: route.osc52,
        osc52_capability: environment.osc52_capability(),
        wrap_sink: snapshot.clipboard.osc52_sink_active,
        display_server: snapshot.display_server,
        container_no_display: snapshot.container_no_display,
        data_control,
        delivery,
        fix: fix.map(str::to_owned),
    }
}

fn issue(warning: TerminalWarning) -> Option<DiagnosticFinding> {
    finding(warning, FindingDisposition::Issue)
}

fn recommendation(warning: TerminalWarning) -> Option<DiagnosticFinding> {
    finding(warning, FindingDisposition::Recommendation)
}

fn finding(warning: TerminalWarning, disposition: FindingDisposition) -> Option<DiagnosticFinding> {
    let id = id_for(warning.category)?;
    Some(DiagnosticFinding {
        id,
        disposition,
        message: warning.message,
        remediation: warning.fix.map(|fix| ManualRemediation {
            fix,
            config_path: warning.config_path,
        }),
        automatic_remediation: (id == crate::diagnostics::SSH_WRAP_ID)
            .then(crate::diagnostics::ssh_wrap_automatic_remediation),
        note: warning.note,
    })
}

pub(crate) const fn id_for(category: WarningCategory) -> Option<DiagnosticId> {
    let item = match category {
        WarningCategory::Clipboard => "tmux-clipboard",
        WarningCategory::DcsPassthrough => "dcs-passthrough",
        WarningCategory::ControlMode => "control-mode",
        WarningCategory::ByobuScreen => "byobu-screen",
        WarningCategory::UnsupportedTerminal => "unsupported-emulator",
        WarningCategory::TmuxExtendedKeysOff => "tmux-extended-keys",
        WarningCategory::WaylandNoDataControl => "wayland-data-control",
        WarningCategory::WezTermKittyKeyboardOff => "wezterm-kitty",
        WarningCategory::LimitedColorSupport => "limited-color",
        WarningCategory::SshWithoutWrap => "ssh-wrap",
        WarningCategory::NotificationProtocolFallback
        | WarningCategory::FocusTrackingUnavailable
        | WarningCategory::SandboxProfileConflict => return None,
    };
    Some(DiagnosticId::new("terminal", item))
}

fn probe_notes(snapshot: &DiagnosticSnapshot<'_>) -> Vec<ProbeNote> {
    let mut notes = Vec::new();
    if snapshot.common.terminal.is_tmux_backed() {
        probe_note(&mut notes, "tmux.version", &snapshot.common.tmux.version);
        probe_note(
            &mut notes,
            "tmux.extended-keys",
            &snapshot.common.tmux.extended_keys,
        );
        probe_note(
            &mut notes,
            "tmux.set-clipboard",
            &snapshot.common.tmux.set_clipboard,
        );
        probe_note(
            &mut notes,
            "tmux.allow-passthrough-support",
            &snapshot.common.tmux.allow_passthrough_support,
        );
        if matches!(
            snapshot.common.tmux.allow_passthrough_support,
            TmuxProbeResult::Available(())
        ) {
            probe_note(
                &mut notes,
                "tmux.allow-passthrough",
                &snapshot.common.tmux.allow_passthrough,
            );
        }
        probe_note(
            &mut notes,
            "tmux.control-mode",
            &snapshot.common.tmux.control_mode,
        );
    }
    runtime_probe_note(
        &mut notes,
        "runtime.fullscreen-active",
        snapshot.runtime.fullscreen_active,
    );
    runtime_probe_note(
        &mut notes,
        "runtime.kitty-flags-pushed",
        snapshot.runtime.kitty_flags_pushed,
    );
    runtime_probe_note(&mut notes, "runtime.xtversion", snapshot.runtime.xtversion);
    runtime_probe_note(&mut notes, "terminal.color", snapshot.color_level);
    if snapshot.common.wayland.is_wayland {
        probe_note(
            &mut notes,
            "wayland.data-control",
            &snapshot.common.wayland.data_control,
        );
    }
    notes
}

fn probe_note<T>(notes: &mut Vec<ProbeNote>, probe: &'static str, result: &TmuxProbeResult<T>) {
    let (status, message) = match result {
        TmuxProbeResult::Available(_) => return,
        TmuxProbeResult::Unsupported => (ProbeStatus::Unsupported, None),
        TmuxProbeResult::Unavailable => (ProbeStatus::Unavailable, None),
        TmuxProbeResult::Error(error) => (ProbeStatus::Error, Some(error.to_owned())),
    };
    notes.push(ProbeNote {
        probe,
        status,
        message,
    });
}

fn runtime_probe_note<T>(
    notes: &mut Vec<ProbeNote>,
    probe: &'static str,
    evidence: RuntimeEvidence<T>,
) {
    if matches!(evidence, RuntimeEvidence::Unavailable) {
        notes.push(ProbeNote {
            probe,
            status: ProbeStatus::Unavailable,
            message: None,
        });
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
