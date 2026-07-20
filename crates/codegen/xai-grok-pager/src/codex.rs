//! `grok codex` — ChatGPT subscription access via the native OpenAI Codex
//! platform backend.
//!
//! Historically this subcommand spawned the external `codex app-server`
//! binary; authentication and inference are now first-party:
//!
//! * login: `grok login --openai` (browser PKCE or `--device-code`), stored
//!   under the `oauth/openai-codex` scope in `~/.grok/auth.json`;
//! * inference: the native sampler speaks the ChatGPT Codex Responses
//!   backend directly (`openai-codex/*` catalog models).
//!
//! This shim rewrites the process arguments so `grok codex` drops into the
//! standard pager flows (TUI, or headless with `-p`) pinned to an
//! `openai-codex/*` model, then gets out of the way.

use crate::app::cli::{CodexArgs, PagerArgs};
use anyhow::{Result, bail};

/// Default Codex subscription model (catalog id form).
pub const DEFAULT_CODEX_MODEL: &str = "openai-codex/gpt-5.6-sol";

/// Outcome of [`rewrite_pager_args`].
pub enum CodexRewrite {
    /// The shim fully handled the invocation (e.g. `--status`); the caller
    /// should exit successfully.
    Handled,
    /// Pager args were rewritten; continue normal startup (TUI or headless).
    Continue,
}

/// Normalize a user-supplied Codex model id to the catalog form
/// `openai-codex/<id>`. Accepts bare ids (`gpt-5.5`) and the legacy
/// app-server prefix (`codex:gpt-5.5`).
fn normalize_model(model: Option<&str>) -> String {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return DEFAULT_CODEX_MODEL.to_owned();
    };
    if let Some(rest) = model.strip_prefix("codex:") {
        return format!("openai-codex/{rest}");
    }
    if model.contains('/') {
        return model.to_owned();
    }
    format!("openai-codex/{model}")
}

/// Rewrite `grok codex …` process args into the equivalent native pager
/// invocation. Runs before subcommand dispatch in `main`.
pub async fn rewrite_pager_args(codex: &CodexArgs, args: &mut PagerArgs) -> Result<CodexRewrite> {
    if codex.status {
        print_status();
        return Ok(CodexRewrite::Handled);
    }
    if let Some(thread) = codex.resume.as_deref() {
        bail!(
            "`grok codex --resume` referred to a Codex app-server thread ({thread}), which the \
             native backend cannot resume. Use Grok sessions instead: `grok sessions` to list, \
             `grok --resume <id>` to continue."
        );
    }
    if codex.codex_binary != std::path::Path::new("codex") {
        eprintln!(
            "note: --codex-binary is deprecated — Grok Build now talks to the ChatGPT Codex \
             backend directly and no external Codex CLI is used."
        );
    }

    // Auto-guide login when unauthenticated (interactive terminals only).
    let home = xai_grok_config::grok_home();
    if xai_grok_shell::auth::read_openai_codex_auth(&home).is_none() {
        use std::io::IsTerminal as _;
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            eprintln!("Not signed in to OpenAI Codex (ChatGPT) — starting login…");
            xai_grok_shell::auth::openai_codex::run_openai_codex_login(
                None,
                xai_grok_shell::auth::openai_codex::CodexLoginMethod::Browser,
            )
            .await?;
        } else {
            bail!(
                "Not signed in to OpenAI Codex (ChatGPT). Run `grok login --openai` \
                 (or `grok login --openai --device-code` if no browser is available)."
            );
        }
    }

    args.command = None;
    if args.model.is_none() {
        args.model = Some(normalize_model(codex.model.as_deref()));
    }
    if codex.full_access {
        args.yolo = true;
    }
    if let Some(prompt) = codex.prompt.clone().or_else(|| codex.message.clone())
        && args.single.is_none()
    {
        args.single = Some(prompt);
    }
    Ok(CodexRewrite::Continue)
}

/// `grok codex --status`: subscription credential + catalog models.
fn print_status() {
    let home = xai_grok_config::grok_home();
    match xai_grok_shell::auth::read_openai_codex_auth(&home) {
        Some(auth) => {
            println!("OpenAI Codex (ChatGPT): signed in");
            if let Some(email) = auth.email.as_deref() {
                println!("  Account: {email}");
            }
            if let Some(account_id) = auth.account_id.as_deref() {
                println!("  Account id: {account_id}");
            }
            match auth.expires_at {
                Some(expiry) => println!("  Token expires: {}", expiry.to_rfc3339()),
                None => println!("  Token expires: unknown"),
            }
        }
        None => {
            println!("OpenAI Codex (ChatGPT): not signed in");
            println!("  Run `grok login --openai` to sign in (browser), or");
            println!("      `grok login --openai --device-code` (headless).");
        }
    }
    println!("Default model: {DEFAULT_CODEX_MODEL}");
    println!("Available models:");
    for model in xai_grok_models::platform_builtin_models()
        .iter()
        .filter(|m| m.platform == xai_grok_models::PlatformId::OpenAiCodex)
    {
        println!("  openai-codex/{}", model.model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_accepts_bare_prefixed_and_legacy_ids() {
        assert_eq!(normalize_model(None), DEFAULT_CODEX_MODEL);
        assert_eq!(normalize_model(Some("gpt-5.5")), "openai-codex/gpt-5.5");
        assert_eq!(
            normalize_model(Some("openai-codex/gpt-5.4")),
            "openai-codex/gpt-5.4"
        );
        assert_eq!(
            normalize_model(Some("codex:gpt-5.4")),
            "openai-codex/gpt-5.4"
        );
    }
}
