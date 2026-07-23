//! Exact planning and application for diagnostic fixes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use xai_grok_config::managed_text::{
    CommentSyntax, ManagedConfig, ManagedConfigPlan, ManagedConfigRequest, ManagedConfigStatus,
    ManagedItem, SyntaxValidator,
};

use crate::diagnostics::{DiagnosticId, DiagnosticReport};
use crate::terminal::TerminalContext;

pub const SSH_WRAP_ID: DiagnosticId = DiagnosticId::new("terminal", "ssh-wrap");
pub const SSH_WRAP_FIX_COMMAND: &str = "grok doctor fix terminal.ssh-wrap";
pub const SSH_WRAP_ONE_OFF: &str = "grok wrap ssh <host>";

const SSH_WRAP_FIX_HANDLE: &str = "ssh-wrap";

const MANAGED_NAMESPACE: &str = "grok doctor";
const SSH_WRAP_ALIAS_POSIX: &str = "alias ssh='grok wrap ssh'";
const SSH_WRAP_ALIAS_FISH: &str = "alias ssh 'grok wrap ssh'";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticRemediation {
    pub fix_id: DiagnosticId,
    pub command: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixRequest {
    pub id: DiagnosticId,
    pub home: PathBuf,
    pub shell: Option<PathBuf>,
    pub validator: Option<PathBuf>,
}

impl FixRequest {
    pub fn from_environment(id: DiagnosticId) -> Result<Self, FixError> {
        let home = actual_home().ok_or(FixError::HomeUnavailable)?;
        let shell = std::env::var_os("SHELL").map(PathBuf::from);
        let validator = shell.as_deref().and_then(resolve_validator_program);
        Ok(Self {
            id,
            home,
            shell,
            validator,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl ShellKind {
    pub fn from_shell_path(shell: &Path) -> Option<Self> {
        match shell.file_name()?.to_str()? {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    pub(crate) fn config_path(self, home: &Path) -> PathBuf {
        match self {
            Self::Bash => home.join(".bashrc"),
            Self::Zsh => home.join(".zshrc"),
            Self::Fish => home.join(".config/fish/config.fish"),
        }
    }

    fn alias(self) -> &'static str {
        match self {
            Self::Bash | Self::Zsh => SSH_WRAP_ALIAS_POSIX,
            Self::Fish => SSH_WRAP_ALIAS_FISH,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedChange {
    pub requested_path: PathBuf,
    pub target_path: PathBuf,
    pub block: String,
    pub backup_path_hint: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct FixPlan {
    pub id: DiagnosticId,
    pub shell: ShellKind,
    pub changes: Vec<PlannedChange>,
    pub caveats: Vec<&'static str>,
    managed: ManagedConfigPlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixStatus {
    Applied,
    AlreadyConfigured,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixOutcome {
    pub id: DiagnosticId,
    pub status: FixStatus,
    pub changed_path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum FixError {
    UnknownId(String),
    PlatformUnsupported,
    HomeUnavailable,
    NotApplicable,
    RemoteSession,
    UnsupportedShell,
    ExistingCustomization { path: PathBuf, detail: String },
    Managed(xai_grok_config::managed_text::ManagedConfigError),
    PostconditionFailed,
}

impl std::fmt::Display for FixError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId(id) => write!(
                formatter,
                "`{id}` is not an available Doctor fix. Run `grok doctor fix` to list available fixes."
            ),
            Self::PlatformUnsupported => write!(
                formatter,
                "Automatic SSH setup is not available on Windows. Run `{SSH_WRAP_ONE_OFF}` when needed."
            ),
            Self::HomeUnavailable => {
                formatter.write_str("Grok could not find your home directory.")
            }
            Self::NotApplicable => {
                formatter.write_str("This fix does not apply to VS Code Remote sessions.")
            }
            Self::RemoteSession => {
                formatter.write_str("Run this fix on your local computer, not in the SSH session.")
            }
            Self::UnsupportedShell => write!(
                formatter,
                "Automatic setup supports Bash, zsh, and fish. For another shell, run `{SSH_WRAP_ONE_OFF}` when needed."
            ),
            Self::ExistingCustomization { path, detail } => write!(
                formatter,
                "Grok found an existing SSH alias or function in {} and did not change it: {detail}",
                path.display()
            ),
            Self::Managed(error) => write!(
                formatter,
                "Could not update your shell configuration: {error}"
            ),
            Self::PostconditionFailed => formatter
                .write_str("The configuration changed, but Grok could not verify the SSH alias."),
        }
    }
}

impl std::error::Error for FixError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Managed(error) => Some(error),
            _ => None,
        }
    }
}

impl From<xai_grok_config::managed_text::ManagedConfigError> for FixError {
    fn from(error: xai_grok_config::managed_text::ManagedConfigError) -> Self {
        Self::Managed(error)
    }
}

pub fn resolve_fix_id(value: &str) -> Result<DiagnosticId, FixError> {
    match value {
        "terminal.ssh-wrap" | SSH_WRAP_FIX_HANDLE => Ok(SSH_WRAP_ID),
        other => Err(FixError::UnknownId(other.to_owned())),
    }
}

pub(crate) fn human_fix_command(id: DiagnosticId) -> Option<String> {
    fix_handle(id).map(|handle| format!("grok doctor fix {handle}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticFixAvailability {
    Here,
    RunLocally,
}

pub(crate) fn select_fix_plan(
    id: DiagnosticId,
    report: &DiagnosticReport,
    terminal: &TerminalContext,
) -> Result<Option<FixPlan>, FixError> {
    if terminal.is_ssh || terminal.is_official_vscode_remote || report.facts.ssh {
        return Ok(None);
    }
    plan_fix(FixRequest::from_environment(id)?, report, terminal).map(Some)
}

pub(crate) fn applicable_automatic_fixes(
    report: &DiagnosticReport,
    terminal: &TerminalContext,
) -> Vec<(DiagnosticId, &'static str, AutomaticFixAvailability)> {
    applicable_automatic_fixes_with(report, terminal, FixRequest::from_environment)
}

fn applicable_automatic_fixes_with(
    report: &DiagnosticReport,
    terminal: &TerminalContext,
    mut request_for: impl FnMut(DiagnosticId) -> Result<FixRequest, FixError>,
) -> Vec<(DiagnosticId, &'static str, AutomaticFixAvailability)> {
    report
        .findings
        .iter()
        .filter_map(|finding| {
            let automatic = finding.automatic_remediation?;
            let handle = fix_handle(automatic.fix_id)?;
            let availability =
                if terminal.is_ssh || terminal.is_official_vscode_remote || report.facts.ssh {
                    AutomaticFixAvailability::RunLocally
                } else {
                    plan_fix(request_for(automatic.fix_id).ok()?, report, terminal).ok()?;
                    AutomaticFixAvailability::Here
                };
            Some((automatic.fix_id, handle, availability))
        })
        .collect()
}

pub(crate) fn format_applicable_automatic_fixes(
    report: &DiagnosticReport,
    terminal: &TerminalContext,
) -> String {
    let fixes = applicable_automatic_fixes(report, terminal);
    if fixes.is_empty() {
        return "No automatic fixes are available here.\n".to_owned();
    }

    let mut output = String::from("Automatic fixes:\n");
    for (_id, handle, availability) in fixes {
        output.push_str(&format!("  {handle:<16} Set up local SSH wrapping\n"));
        match availability {
            AutomaticFixAvailability::Here => output.push_str(&format!(
                "    Run: grok doctor fix {handle}\n    In Grok: /doctor fix {handle}\n"
            )),
            AutomaticFixAvailability::RunLocally => {
                output.push_str(&format!(
                    "    On your local computer, run: grok doctor fix {handle}\n"
                ));
            }
        }
    }
    output
}

pub(crate) fn format_fix_preview(plan: &FixPlan) -> String {
    use std::fmt::Write as _;

    let mut output = String::from("Doctor Fix\n\n");
    let _ = writeln!(output, "Fix: {}", plan.id);
    let _ = writeln!(output, "Shell: {}", plan.shell.name());
    for change in &plan.changes {
        let _ = writeln!(output, "File: {}", change.requested_path.display());
        if change.target_path != change.requested_path {
            let _ = writeln!(
                output,
                "Actual file: {} (symlink target)",
                change.target_path.display()
            );
        }
        let _ = writeln!(output, "\nText to add:\n{}", change.block);
        match &change.backup_path_hint {
            Some(path) => {
                let _ = writeln!(
                    output,
                    "\nBackup will be saved to: {}\nIf that file exists, Grok will choose a unique name.",
                    path.display()
                );
            }
            None => output.push_str("\nBackup: None. The file is new or no changes are needed.\n"),
        }
    }
    output.push_str(
        "\nWhat this changes:\n  In new interactive shells, `ssh ...` runs as `grok wrap ssh ...`.\n",
    );
    let _ = writeln!(
        output,
        "  To use once without changing config: `{SSH_WRAP_ONE_OFF}`."
    );
    output.push_str("Caveats:\n");
    for caveat in &plan.caveats {
        let _ = writeln!(output, "  - {caveat}");
    }
    output
}

fn fix_handle(id: DiagnosticId) -> Option<&'static str> {
    (id == SSH_WRAP_ID).then_some(SSH_WRAP_FIX_HANDLE)
}

pub fn plan_fix(
    request: FixRequest,
    report: &DiagnosticReport,
    terminal: &TerminalContext,
) -> Result<FixPlan, FixError> {
    if request.id != SSH_WRAP_ID {
        return Err(FixError::UnknownId(request.id.to_string()));
    }
    if cfg!(windows) {
        return Err(FixError::PlatformUnsupported);
    }
    if terminal.is_official_vscode_remote {
        return Err(FixError::NotApplicable);
    }
    if terminal.is_ssh || report.facts.ssh {
        return Err(FixError::RemoteSession);
    }

    let shell = request
        .shell
        .as_deref()
        .and_then(ShellKind::from_shell_path)
        .ok_or(FixError::UnsupportedShell)?;
    let path = shell.config_path(&request.home);
    let validator = validator_for(shell, request.validator);
    let managed = ManagedConfig::plan(ManagedConfigRequest {
        path,
        namespace: MANAGED_NAMESPACE.to_owned(),
        owned_item_prefix: "terminal.".to_owned(),
        items: vec![ManagedItem::new(request.id.to_string(), shell.alias())],
        comments: CommentSyntax::hash(),
        validator,
    })?;
    if let Some(detail) = detect_ssh_customization(managed.inspection().unmanaged_text(), shell) {
        return Err(FixError::ExistingCustomization {
            path: managed.target_path().to_path_buf(),
            detail,
        });
    }
    let block = managed
        .managed_block()
        .ok_or(FixError::PostconditionFailed)?;
    let change = PlannedChange {
        requested_path: managed.requested_path().to_path_buf(),
        target_path: managed.target_path().to_path_buf(),
        block,
        backup_path_hint: managed.backup_path_hint().map(Path::to_path_buf),
    };
    Ok(FixPlan {
        id: request.id,
        shell,
        changes: vec![change],
        caveats: vec![
            "The alias loads only in new interactive shells.",
            "Use `command ssh ...` to bypass the alias.",
            "For manually entered `ssh -f`, ControlPersist workflows, or OpenSSH `~^Z` local suspend, use `command ssh ...`. Wrapping does not fully preserve those behaviors.",
            "`grok wrap` starts the SSH process directly, so the alias does not loop.",
            "Grok checks this file for direct SSH aliases and functions. Review sourced files, plugins, and generated shell setup yourself.",
        ],
        managed,
    })
}

pub fn apply_fix(plan: FixPlan) -> Result<FixOutcome, FixError> {
    let id = plan.id;
    let shell = plan.shell;
    let outcome = ManagedConfig::apply(plan.managed)?;
    if !managed_alias_configured(&outcome.target_path, shell) {
        return Err(FixError::PostconditionFailed);
    }
    Ok(FixOutcome {
        id,
        status: match outcome.status {
            ManagedConfigStatus::Applied => FixStatus::Applied,
            ManagedConfigStatus::NoChange => FixStatus::AlreadyConfigured,
        },
        changed_path: outcome.requested_path,
        backup_path: outcome.backup_path,
    })
}

pub fn ssh_wrap_automatic_remediation() -> AutomaticRemediation {
    AutomaticRemediation {
        fix_id: SSH_WRAP_ID,
        command: SSH_WRAP_FIX_COMMAND,
    }
}

pub fn managed_alias_configured(path: &Path, shell: ShellKind) -> bool {
    let request = ManagedConfigRequest {
        path: path.to_path_buf(),
        namespace: MANAGED_NAMESPACE.to_owned(),
        owned_item_prefix: "terminal.".to_owned(),
        items: vec![ManagedItem::new(SSH_WRAP_ID.to_string(), shell.alias())],
        comments: CommentSyntax::hash(),
        validator: None,
    };
    ManagedConfig::plan(request).is_ok_and(|plan| {
        !plan.changes_file()
            && detect_ssh_customization(plan.inspection().unmanaged_text(), shell).is_none()
    })
}

fn validator_for(_shell: ShellKind, override_path: Option<PathBuf>) -> Option<SyntaxValidator> {
    let program = override_path?;
    Some(SyntaxValidator {
        program,
        args: vec!["-n".into()],
        timeout: Duration::from_secs(2),
    })
}

fn resolve_validator_program(shell: &Path) -> Option<PathBuf> {
    let kind = ShellKind::from_shell_path(shell)?;
    if shell.components().count() > 1 {
        return executable_file(shell).then(|| shell.to_path_buf());
    }
    find_on_path(kind.name())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    find_on_path_in(name, std::env::split_paths(&std::env::var_os("PATH")?))
}

fn find_on_path_in<P>(name: &str, directories: impl IntoIterator<Item = P>) -> Option<PathBuf>
where
    P: AsRef<Path>,
{
    directories
        .into_iter()
        .map(|directory| directory.as_ref().join(name))
        .find(|candidate| executable_file(candidate))
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

fn detect_ssh_customization(text: &str, shell: ShellKind) -> Option<String> {
    match shell {
        ShellKind::Bash | ShellKind::Zsh => detect_posix_ssh_customization(text),
        ShellKind::Fish => detect_fish_ssh_customization(text),
    }
}

fn detect_posix_ssh_customization(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if is_posix_ssh_alias_declaration(trimmed) {
            return Some("existing `alias ssh=...`".to_owned());
        }
        if is_posix_ssh_function_declaration(trimmed) {
            return Some("existing `ssh` shell function".to_owned());
        }
    }
    None
}

fn is_posix_ssh_alias_declaration(line: &str) -> bool {
    let Some(after_name) =
        after_shell_keyword(line, "alias").and_then(|rest| rest.strip_prefix("ssh"))
    else {
        return false;
    };
    after_name.starts_with('=')
        || (after_name.chars().next().is_some_and(char::is_whitespace)
            && after_name.trim_start().starts_with('='))
}

fn is_posix_ssh_function_declaration(line: &str) -> bool {
    let after_function = after_shell_keyword(line, "function");
    if after_function.is_some_and(|rest| token_is_exact_name(rest, "ssh")) {
        return true;
    }

    let Some(after_name) = line.strip_prefix("ssh") else {
        return false;
    };
    let after_name = after_name.trim_start();
    after_name.starts_with("()")
        || (after_name.starts_with('(') && after_name[1..].trim_start().starts_with(')'))
}

fn token_is_exact_name(text: &str, name: &str) -> bool {
    let Some(rest) = text.strip_prefix(name) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() || ch == '(' || ch == '{')
}

fn after_shell_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then_some(rest.trim_start())
}

fn detect_fish_ssh_customization(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if after_shell_keyword(trimmed, "alias").is_some_and(|rest| {
            token_is_exact_name(rest, "ssh")
                || rest
                    .strip_prefix("ssh")
                    .is_some_and(|rest| rest.starts_with('='))
        }) {
            return Some("existing `alias ssh ...`".to_owned());
        }
        if after_shell_keyword(trimmed, "function")
            .is_some_and(|rest| token_is_exact_name(rest, "ssh"))
        {
            return Some("existing `ssh` fish function".to_owned());
        }
    }
    None
}

fn actual_home() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
}

pub fn configured_report(mut report: DiagnosticReport, configured: bool) -> DiagnosticReport {
    if configured {
        report.findings.retain(|finding| finding.id != SSH_WRAP_ID);
    }
    report
}

#[cfg(test)]
pub(crate) fn test_fix_plan(home: &Path) -> FixPlan {
    plan_fix(
        tests::request(home, "/bin/bash"),
        &tests::report(),
        &TerminalContext::default(),
    )
    .unwrap()
}

#[cfg(test)]
#[path = "fix_tests.rs"]
mod tests;
