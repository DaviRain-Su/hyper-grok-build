//! Settings → Hyper: monorepo agent integration for the local-link desktop.
//!
//! Desktop sessions live under the comet data dir; agent auth, memory, skills,
//! workflows, and WASM extensions are owned by the Hyper CLI (`~/.grok` /
//! `GROK_HOME`). This page surfaces that boundary and first-run actions:
//! ensure CLI (download when missing) and agent login.

use std::path::PathBuf;

use gpui::{
    Context, Entity, SharedString, Task, Window, div, prelude::*, px,
};

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

enum EnsureState {
    Idle,
    Working,
    Ok(String),
    Err(String),
}

pub struct HyperPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
    hyper_bin: Option<PathBuf>,
    hyper_err: Option<String>,
    grok_home: PathBuf,
    data_dir: Option<PathBuf>,
    managed_bin: PathBuf,
    ensure: EnsureState,
    ensure_task: Option<Task<()>>,
}

impl HyperPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let data_dir = state.read(cx).data_dir.clone();
        let (hyper_bin, hyper_err) = match comet_harness::resolve_hyper_bin() {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let grok_home = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".grok")
            });
        let managed_bin = comet_harness::default_desktop_bin_dir().join("hyper");
        Self {
            state,
            hyper_bin,
            hyper_err,
            grok_home,
            data_dir,
            managed_bin,
            ensure: EnsureState::Idle,
            ensure_task: None,
        }
    }

    fn refresh_bin(&mut self) {
        match comet_harness::resolve_hyper_bin() {
            Ok(p) => {
                self.hyper_bin = Some(p);
                self.hyper_err = None;
            }
            Err(e) => {
                self.hyper_bin = None;
                self.hyper_err = Some(e.to_string());
            }
        }
    }

    fn ensure_cli(&mut self, cx: &mut Context<Self>) {
        if matches!(self.ensure, EnsureState::Working) {
            return;
        }
        self.ensure = EnsureState::Working;
        self.ensure_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { comet_harness::ensure_hyper_bin().await })
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(path) => {
                        page.ensure = EnsureState::Ok(path.display().to_string());
                        page.refresh_bin();
                    }
                    Err(e) => {
                        page.ensure = EnsureState::Err(e.to_string());
                    }
                }
                page.ensure_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for HyperPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let bin_line = self
            .hyper_bin
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                self.hyper_err
                    .clone()
                    .unwrap_or_else(|| "not found".into())
            });
        let data = self
            .data_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not set)".into());
        let missing = self.hyper_bin.is_none();
        let ensure_label = match &self.ensure {
            EnsureState::Idle if missing => "Download Hyper CLI",
            EnsureState::Idle => "Re-check / update CLI",
            EnsureState::Working => "Downloading…",
            EnsureState::Ok(_) => "Installed",
            EnsureState::Err(_) => "Retry download",
        };
        let ensure_status = match &self.ensure {
            EnsureState::Ok(p) => Some(format!("Ready: {p}")),
            EnsureState::Err(e) => Some(format!("Failed: {e}")),
            EnsureState::Working => Some("Fetching latest release from GitHub…".into()),
            EnsureState::Idle if missing => Some(format!(
                "Will install to {}",
                self.managed_bin.display()
            )),
            EnsureState::Idle => None,
        };
        let working = matches!(self.ensure, EnsureState::Working);

        widgets::page_column()
            .id("hyper-integration-page")
            .child(widgets::page_header(&theme, "Hyper", None))
            .child(widgets::page_subtitle(
                &theme,
                "Local-link agent bridge — desktop shell drives Hyper over ACP",
            ))
            .child(
                widgets::section_card(&theme)
                    .child(card_row(
                        &theme,
                        "Hyper binary",
                        &bin_line,
                    ))
                    .child(card_row(
                        &theme,
                        "Managed install path",
                        &self.managed_bin.display().to_string(),
                    ))
                    .child(card_row(&theme, "Desktop data dir", &data))
                    .child(card_row(
                        &theme,
                        "Agent home (GROK_HOME)",
                        &self.grok_home.display().to_string(),
                    ))
                    .child(
                        div()
                            .px(px(16.0))
                            .py(px(14.0))
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("hyper-ensure-cli")
                                    .hover(widgets::ghost_hover)
                                    .when(working, |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ensure_cli(cx);
                                    }))
                                    .child(SharedString::from(ensure_label)),
                            )
                            .children(ensure_status.map(|s| {
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(s))
                            })),
                    ),
            )
            .child(
                widgets::section_card(&theme)
                    .child(card_block(
                        &theme,
                        "First-run setup",
                        "1. If Hyper CLI is missing, click Download above (or it auto-downloads \
                         on first chat / model load / Accounts login).\n\
2. Open Settings → Accounts → Hyper → Add account to sign in (OAuth).\n\
3. Models come from the live agent catalog once the CLI is present and authenticated.",
                    ))
                    .child(card_block(
                        &theme,
                        "What lives where",
                        "Desktop: spaces, chats, UI layout under the data dir.\n\
Hyper: login (~/.grok/auth.json), memory, skills, plugins, WASM extensions, Rhai workflows.\n\
Sessions are not merged with the Hyper TUI transcript store — same agent identity, separate chat lists.",
                    ))
                    .child(card_block(
                        &theme,
                        "Workflows & extensions",
                        "In a desktop chat, Hyper accepts the same slash commands as the TUI, \
including `/workflow` (list / run / pause).\n\
WASM plugins and marketplace install go through Hyper config; the desktop does not host a second runtime.",
                    ))
                    .child(card_block(
                        &theme,
                        "Tips",
                        "• Prefer ./scripts/run-desktop.sh from the monorepo root.\n\
• Settings → Accounts → Hyper for GUI login, or: comet agent-login.\n\
• Override agent path with HYPER_AGENT_BIN.\n\
• Set GROK_HOME to share agent home with a custom Hyper install.",
                    )),
            )
    }
}

fn card_row(theme: &Theme, key: &str, value: &str) -> gpui::Div {
    div()
        .px(px(16.0))
        .py(px(12.0))
        .border_b_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(key.to_string())),
        )
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme.text)
                .child(SharedString::from(value.to_string())),
        )
}

fn card_block(theme: &Theme, title: &str, body: &str) -> gpui::Div {
    div()
        .px(px(16.0))
        .py(px(14.0))
        .border_b_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(body.to_string())),
        )
}
