//! Settings → Hyper: monorepo agent integration for the local-link desktop.
//!
//! Desktop sessions live under the comet data dir; agent auth, memory, skills,
//! workflows, and WASM extensions are owned by the Hyper CLI (`~/.grok` /
//! `GROK_HOME`). This page surfaces that boundary and how to use it.

use std::path::PathBuf;

use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, px,
};

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct HyperPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
    hyper_bin: Option<PathBuf>,
    hyper_err: Option<String>,
    grok_home: PathBuf,
    data_dir: Option<PathBuf>,
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
        Self {
            state,
            hyper_bin,
            hyper_err,
            grok_home,
            data_dir,
        }
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
                    .child(card_row(&theme, "Desktop data dir", &data))
                    .child(card_row(
                        &theme,
                        "Agent home (GROK_HOME)",
                        &self.grok_home.display().to_string(),
                    )),
            )
            .child(
                widgets::section_card(&theme)
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
• comet agent-login → Hyper OAuth.\n\
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
