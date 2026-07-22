use std::io::{IsTerminal as _, Write};
use std::path::Path;

use anyhow::Result;

use crate::diagnostics::{DiagnosticReport, FixPlan, FixStatus, ShellKind};

mod human;
mod json;

pub const SCHEMA_VERSION: &str = "1";

#[derive(Clone, Debug, Default, Eq, PartialEq, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DoctorArgs {
    /// Emit machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<DoctorCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq, clap::Subcommand)]
pub enum DoctorCommand {
    /// Apply a named automatic remediation.
    Fix(FixArgs),
}

#[derive(Clone, Debug, Eq, PartialEq, clap::Args)]
pub struct FixArgs {
    /// Short fix handle (`ssh-wrap`); canonical `terminal.ssh-wrap` is also accepted.
    pub id: String,
    /// Apply without prompting after printing the exact plan.
    #[arg(long)]
    pub yes: bool,
}

pub fn run(args: DoctorArgs) -> Result<()> {
    match args.command {
        None => run_report(args.json, &mut std::io::stdout().lock()),
        Some(DoctorCommand::Fix(fix)) => run_fix(
            fix,
            std::io::stdin().is_terminal(),
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        ),
    }
}

pub fn run_with_writer(args: DoctorArgs, writer: &mut impl Write) -> Result<()> {
    match args.command {
        None => run_report(args.json, writer),
        Some(_) => anyhow::bail!("doctor fixes require interactive input/output"),
    }
}

fn run_report(json_output: bool, writer: &mut impl Write) -> Result<()> {
    let report = collect_report();
    write_report(&report, json_output, writer)
}

pub fn collect_report() -> DiagnosticReport {
    let terminal = crate::terminal::standalone_terminal_context();
    let report = collect_report_with(crate::diagnostics::probes::collect_standalone(&terminal));
    configured_report_for_terminal(report, &terminal)
}

fn configured_report_for_terminal(
    report: DiagnosticReport,
    terminal: &crate::terminal::TerminalContext,
) -> DiagnosticReport {
    let configured = shell_home_and_kind()
        .map(|(home, shell)| {
            crate::diagnostics::managed_alias_configured(&shell.config_path(&home), shell)
        })
        .unwrap_or(false);
    if terminal.is_ssh || terminal.is_official_vscode_remote {
        report
    } else {
        crate::diagnostics::configured_report(report, configured)
    }
}

fn collect_report_with(
    snapshot: crate::diagnostics::probes::StandaloneDiagnosticSnapshot<'_>,
) -> DiagnosticReport {
    let mut report = crate::diagnostics::view(snapshot.into());
    // Passive mic fact when audio is compiled in. No issue finding — headless
    // hosts often have no input device; the Voice fact row is enough.
    crate::diagnostics::apply_voice_probe(&mut report, false);
    report
}

fn write_report(
    report: &DiagnosticReport,
    json_output: bool,
    writer: &mut impl Write,
) -> Result<()> {
    if json_output {
        json::write(report, writer)
    } else {
        write!(writer, "{}", human::format(report))?;
        Ok(())
    }
}

fn run_fix(
    args: FixArgs,
    stdin_is_terminal: bool,
    input: &mut impl std::io::BufRead,
    writer: &mut impl Write,
) -> Result<()> {
    let id = crate::diagnostics::resolve_fix_id(&args.id)?;
    let terminal = crate::terminal::standalone_terminal_context();
    let report = configured_report_for_terminal(
        collect_report_with(crate::diagnostics::probes::collect_standalone(&terminal)),
        &terminal,
    );
    let request = crate::diagnostics::FixRequest::from_environment(id)?;
    let plan = crate::diagnostics::plan_fix(request, &report, &terminal)?;
    apply_fix_plan(args, stdin_is_terminal, input, writer, &terminal, plan)
}

fn apply_fix_plan(
    args: FixArgs,
    stdin_is_terminal: bool,
    input: &mut impl std::io::BufRead,
    writer: &mut impl Write,
    terminal: &crate::terminal::TerminalContext,
    plan: FixPlan,
) -> Result<()> {
    let id = plan.id;
    write_fix_preview(&plan, writer)?;

    if !args.yes {
        if !stdin_is_terminal {
            anyhow::bail!(
                "refusing to apply a doctor fix from non-interactive stdin without --yes"
            );
        }
        write!(writer, "\nApply this change? [y/N] ")?;
        writer.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(writer, "Cancelled.")?;
            return Ok(());
        }
    }

    let shell = plan.shell;
    let outcome = crate::diagnostics::apply_fix(plan)?;
    let post_report = crate::diagnostics::configured_report(
        collect_report_with(crate::diagnostics::probes::collect_standalone(terminal)),
        crate::diagnostics::managed_alias_configured(&outcome.changed_path, shell),
    );
    if post_report.findings.iter().any(|finding| finding.id == id) {
        anyhow::bail!("fix applied, but `{id}` is still reported");
    }

    match outcome.status {
        FixStatus::Applied => writeln!(
            writer,
            "\nConfigured {id} in {}.",
            outcome.changed_path.display()
        )?,
        FixStatus::AlreadyConfigured => writeln!(
            writer,
            "\n{id} is already configured in {}.",
            outcome.changed_path.display()
        )?,
    }
    if let Some(backup) = outcome.backup_path {
        writeln!(writer, "Backup: {}", backup.display())?;
    }
    writeln!(writer, "Open a new interactive shell to use the alias.")?;
    Ok(())
}

fn write_fix_preview(plan: &FixPlan, writer: &mut impl Write) -> std::io::Result<()> {
    writeln!(writer, "Doctor fix: {}", plan.id)?;
    writeln!(writer, "Shell: {}", plan.shell.name())?;
    for change in &plan.changes {
        writeln!(writer, "File: {}", change.requested_path.display())?;
        if change.target_path != change.requested_path {
            writeln!(writer, "Physical target: {}", change.target_path.display())?;
        }
        writeln!(writer, "\nManaged block:")?;
        writeln!(writer, "{}", change.block)?;
        match &change.backup_path_hint {
            Some(path) => writeln!(
                writer,
                "\nProposed backup: {} (apply retries a nearby unique name on collision)",
                path.display()
            )?,
            None => writeln!(writer, "\nBackup: none (new file or exact no-op)")?,
        }
    }
    writeln!(writer, "\nBehavior:")?;
    writeln!(
        writer,
        "  New interactive shells run typed `ssh ...` as `grok wrap ssh ...`."
    )?;
    writeln!(
        writer,
        "  One-off alternative without changing config: `{}`.",
        crate::diagnostics::SSH_WRAP_ONE_OFF
    )?;
    writeln!(writer, "Caveats:")?;
    for caveat in &plan.caveats {
        writeln!(writer, "  - {caveat}")?;
    }
    Ok(())
}

fn shell_home_and_kind() -> Option<(std::path::PathBuf, ShellKind)> {
    #[allow(deprecated)]
    let home = std::env::home_dir()?;
    let shell = std::env::var_os("SHELL")?;
    let kind = ShellKind::from_shell_path(Path::new(&shell))?;
    Some((home, kind))
}

#[cfg(test)]
mod tests;
