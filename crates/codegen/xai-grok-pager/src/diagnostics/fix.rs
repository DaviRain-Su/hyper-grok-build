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

#[derive(Debug)]
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
            Self::UnknownId(id) => write!(formatter, "unknown diagnostic fix `{id}`"),
            Self::PlatformUnsupported => write!(
                formatter,
                "automatic SSH alias setup is not supported on Windows; use `{SSH_WRAP_ONE_OFF}` manually"
            ),
            Self::HomeUnavailable => {
                formatter.write_str("cannot determine the actual user home directory")
            }
            Self::NotApplicable => formatter
                .write_str("this fix is not applicable in official VS Code Remote sessions"),
            Self::RemoteSession => formatter
                .write_str("run this fix on your local machine, not inside the SSH session"),
            Self::UnsupportedShell => write!(
                formatter,
                "automatic setup supports Bash, zsh, and fish; use `{SSH_WRAP_ONE_OFF}` manually"
            ),
            Self::ExistingCustomization { path, detail } => write!(
                formatter,
                "existing SSH alias/function found in {}; it was not overwritten: {detail}",
                path.display()
            ),
            Self::Managed(error) => write!(formatter, "managed config update failed: {error}"),
            Self::PostconditionFailed => formatter
                .write_str("fix applied, but the configured SSH alias could not be verified"),
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
            "The alias is loaded only by new interactive shell sessions.",
            "Use `command ssh ...` to bypass the alias.",
            "For manually typed `ssh -f`, ControlPersist workflows, or OpenSSH `~^Z` local suspend, use `command ssh ...`; wrapping is not fully transparent for those cases.",
            "`grok wrap` spawns the real SSH process directly, so the alias does not recurse.",
            "Conflict detection covers direct alias/function declarations in this file only; sourced files, plugins, and dynamic shell setup require manual review.",
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
#[path = "fix_tests.rs"]
mod tests;
