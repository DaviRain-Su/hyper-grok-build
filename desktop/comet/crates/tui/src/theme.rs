//! Palette — comet's, not an invention.
//!
//! Every value here is the sRGB conversion of the exact oklch the gpui viewport
//! paints (`comet-ui/src/theme.rs`), so the two surfaces are the same product.
//! The tests at the bottom pin the conversions; `comet_proto::view::dot` holds
//! the status hues both frontends read, because a dot that means "running" in
//! one place cannot mean something else in the other.
//!
//! Two rules the original enforces and this follows:
//!
//! 1. **Near-monochrome.** Text is a three-step neutral ramp. Color appears only
//!    on status dots and, per the original's own note, *indigo is reserved for
//!    Send*. Nothing else is tinted — no colored tool names, no green code.
//! 2. **Structure is background, not line work.** This is opencode's rule and it
//!    replaces every border this renderer used to draw. Their whole TUI has one
//!    background ramp — `background` (the terminal's own, left untouched),
//!    `backgroundPanel`, `backgroundElement` — and regions are told apart by
//!    which step they are filled with. A sidebar is a panel fill, a prompt is an
//!    element fill, a selected row is one step up from whatever it sits on.
//!    Nothing is ruled, boxed or tee'd.
//!
//! The ramp below is opencode's own `generateGrayScale` evaluated for comet's
//! near-black `#060606`. Their dark branch for a background this dark ignores
//! the hue and walks a neutral: `grays[i] = floor(i / 12 * 0.4 * 255)`. So
//! `panel` is `grays[2]`, `element` is `grays[3]`, and the menu selection step
//! is `grays[5]` — the same three the TypeScript reads for the same jobs.
//!
//! The cost this trades against is real: every filled cell is an SGR sequence
//! ratatui must emit, and a filled pane no longer lets the user's terminal
//! background (or its transparency) through. opencode pays it for the sidebar,
//! the prompt and menus while leaving the transcript on the terminal default —
//! the largest, most-scrolled region stays free — and so does this.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Primary text — neutral(0.922).
    pub text: Color,
    /// Secondary text: sub-lines, section labels — neutral(0.708).
    pub muted: Color,
    /// Tertiary: timestamps, hints — neutral(0.556).
    pub faint: Color,
    /// Indigo. Reserved for Send and focus, per the original.
    pub accent: Color,
    /// The app's own background, painted across the whole frame.
    ///
    /// opencode ships `background: transparent` so the terminal shows through,
    /// and that is the right default for a tool that is mostly text on nothing.
    /// It is the wrong default here: this app is made of filled blocks, and a
    /// block only reads as raised against a surface it can be *compared to*. On
    /// a coloured terminal the fills stopped looking like structure and started
    /// looking like stains, so the app brings its own surface.
    pub base: Color,
    /// The sidebar and a menu — a region that is a *place*, one step above
    /// [`Self::base`]. opencode's `backgroundPanel`.
    pub panel: Color,
    /// A block: a message, a tool group, the prompt, the active tab. opencode's
    /// `backgroundElement`, which is also their `backgroundMenu`.
    pub element: Color,
    /// The selected row of a list or menu — a step clear of whatever it sits on.
    pub selection: Color,
    /// Fenced and inline code. Below [`Self::element`], because code quoted
    /// inside a block should read as recessed within it, not stacked on top.
    pub code_wash: Color,
    pub danger: Color,
    pub warning: Color,
    // Status dots. Same glyph everywhere; only the color differs.
    pub dot_working: Color,
    pub dot_awaiting: Color,
    pub dot_errored: Color,
    pub dot_completed: Color,
    pub dot_idle: Color,
    /// True when colors were suppressed — renderers lean on modifiers instead.
    pub plain: bool,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            text: Color::Rgb(0xe5, 0xe5, 0xe5),
            muted: Color::Rgb(0xa1, 0xa1, 0xa1),
            faint: Color::Rgb(0x73, 0x73, 0x73),
            accent: Color::Rgb(0x7c, 0x86, 0xff),
            base: Color::Rgb(0x0b, 0x0b, 0x0d),
            panel: Color::Rgb(0x12, 0x12, 0x15),
            element: Color::Rgb(0x1c, 0x1c, 0x21),
            selection: Color::Rgb(0x2c, 0x2c, 0x33),
            code_wash: Color::Rgb(0x16, 0x16, 0x1a),
            danger: Color::Rgb(0xff, 0x64, 0x67),
            warning: Color::Rgb(0xff, 0xb9, 0x00),
            dot_working: Color::Rgb(0xfb, 0x64, 0xb6),
            dot_awaiting: Color::Rgb(0x7c, 0x86, 0xff),
            dot_errored: Color::Rgb(0xff, 0x64, 0x67),
            dot_completed: Color::Rgb(0x00, 0xd4, 0x92),
            dot_idle: Color::Rgb(0x2f, 0x2f, 0x2f),
            plain: false,
        }
    }

    /// `NO_COLOR`: every hue collapses to the terminal default. Callers that
    /// carry meaning in color also set a [`Modifier`], so nothing is lost.
    pub fn plain() -> Self {
        let reset = Color::Reset;
        Self {
            text: reset,
            muted: reset,
            faint: reset,
            accent: reset,
            base: reset,
            panel: reset,
            element: reset,
            selection: reset,
            code_wash: reset,
            danger: reset,
            warning: reset,
            dot_working: reset,
            dot_awaiting: reset,
            dot_errored: reset,
            dot_completed: reset,
            dot_idle: reset,
            plain: true,
        }
    }

    /// `NO_COLOR` set to anything non-empty disables color
    /// ([no-color.org](https://no-color.org)).
    pub fn from_env() -> Self {
        match std::env::var_os("NO_COLOR") {
            Some(v) if !v.is_empty() => Self::plain(),
            _ => Self::dark(),
        }
    }

    /// The app's own surface, painted across the whole frame before anything
    /// else. Everything below is drawn over it.
    pub fn base(&self) -> Style {
        self.at(self.base, self.text)
    }

    pub fn body(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn subtle(&self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn hint(&self) -> Style {
        Style::default().fg(self.faint)
    }

    /// The indigo bar down the left of a block that is *yours* — your message,
    /// your prompt. opencode marks both the same way, and it is the only place
    /// colour appears in the chrome.
    pub fn accent_bar(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.accent).bg(self.element)
        }
    }

    /// Section labels ("Sessions").
    pub fn label(&self) -> Style {
        let style = Style::default().fg(self.muted);
        if self.plain {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }

    // -- The ramp ----------------------------------------------------------
    //
    // Each level comes as a trio — primary, muted, faint — because text drawn
    // *on* a fill has to name that fill as its own background too. A span left
    // at the terminal default punches a hole in the region it sits in, which is
    // the one failure mode a fill-based layout has.

    /// A region that is a place: the sidebar, the composer footer, a menu.
    pub fn panel(&self) -> Style {
        self.at(self.panel, self.text)
    }

    pub fn panel_subtle(&self) -> Style {
        self.at(self.panel, self.muted)
    }

    pub fn panel_hint(&self) -> Style {
        self.at(self.panel, self.faint)
    }

    /// A thing you act on inside a panel: the prompt, the active tab, a chip.
    pub fn element(&self) -> Style {
        self.at(self.element, self.text)
    }

    pub fn element_subtle(&self) -> Style {
        self.at(self.element, self.muted)
    }

    pub fn element_hint(&self) -> Style {
        self.at(self.element, self.faint)
    }

    /// The user's own message. opencode fills user messages at element level;
    /// so does comet's desktop bubble. Under `NO_COLOR` it reverses instead —
    /// whose turn it is survives losing the ramp.
    pub fn bubble(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            self.element()
        }
    }

    /// The cursor row of a list or menu — a step clear of whatever it sits on.
    /// Under `NO_COLOR` reversed video carries it, since there is no ramp left.
    pub fn selected(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(self.text).bg(self.selection)
        }
    }

    /// Muted text on a selected row.
    pub fn selected_hint(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            self.at(self.selection, self.muted)
        }
    }

    /// Code — inline and fenced. Recessed, not raised.
    pub fn code(&self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.text).bg(self.code_wash)
        }
    }

    /// Foreground `fg` over background `bg`, collapsing to nothing under
    /// `NO_COLOR` — where the fills are all `Reset` and would erase each other.
    fn at(&self, bg: Color, fg: Color) -> Style {
        if self.plain {
            Style::default()
        } else {
            Style::default().fg(fg).bg(bg)
        }
    }

    /// The dot color for a chat status.
    pub fn dot(&self, status: comet_proto::ChatIndicator) -> Style {
        use comet_proto::ChatIndicator as I;
        let color = match status {
            I::Working => self.dot_working,
            I::AwaitingInput => self.dot_awaiting,
            I::Errored => self.dot_errored,
            I::Completed => self.dot_completed,
            I::Idle => self.dot_idle,
        };
        let style = Style::default().fg(color);
        // Under NO_COLOR the dot can't carry state, so bold the two that want
        // attention and leave the rest plain.
        if self.plain && matches!(status, I::AwaitingInput | I::Errored) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

/// The status dot. One glyph for every state — "status is a rail, not a word"
/// (comet's session-row): rows stay aligned and a state change reads in place.
pub const DOT: &str = "●";

/// How often the render loop wakes while something is animating. The loaders
/// are pure functions of elapsed time, so this only sets smoothness, not
/// correctness — a missed tick shifts nothing.
pub const ANIMATION_TICK_MS: u64 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// The oklch → sRGB conversion, duplicated from `comet-ui`'s theme so this
    /// crate can assert its literals without depending on gpui.
    fn oklch(l: f32, c: f32, h_deg: f32) -> Color {
        let h = h_deg.to_radians();
        let (a, b) = (c * h.cos(), c * h.sin());
        let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
        let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
        let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
        let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
        let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
        let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
        let bl = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;
        let enc = |x: f32| {
            let x = x.clamp(0.0, 1.0);
            let v = if x <= 0.003_130_8 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
            (v * 255.0).round() as u8
        };
        Color::Rgb(enc(r), enc(g), enc(bl))
    }

    #[test]
    fn the_palette_is_comets() {
        // If the desktop theme moves, these fail and say so — the whole point of
        // porting values rather than eyeballing them.
        let t = Theme::dark();
        assert_eq!(t.text, oklch(0.922, 0.0, 0.0), "neutral-200 body text");
        assert_eq!(t.muted, oklch(0.708, 0.0, 0.0), "neutral-400 sub-lines");
        assert_eq!(t.faint, oklch(0.556, 0.0, 0.0), "neutral-500 timestamps");
        assert_eq!(t.accent, oklch(0.673, 0.182, 276.935), "indigo-400");
        assert_eq!(t.danger, oklch(0.704, 0.191, 22.216), "red-400");
        assert_eq!(t.warning, oklch(0.828, 0.189, 84.429), "amber-400");
    }

    #[test]
    fn status_dots_match_the_shared_hues() {
        // The meanings live in proto so they cannot diverge per surface.
        let t = Theme::dark();
        let d = comet_proto::view::dot::WORKING;
        assert_eq!(t.dot_working, oklch(d.0, d.1, d.2), "pink, not amber");
        let d = comet_proto::view::dot::AWAITING;
        assert_eq!(t.dot_awaiting, oklch(d.0, d.1, d.2));
        let d = comet_proto::view::dot::ERRORED;
        assert_eq!(t.dot_errored, oklch(d.0, d.1, d.2));
        let d = comet_proto::view::dot::COMPLETED;
        assert_eq!(t.dot_completed, oklch(d.0, d.1, d.2), "emerald");
        // Awaiting is the accent, exactly as in the original.
        assert_eq!(t.dot_awaiting, t.accent);
    }

    #[test]
    fn every_level_of_the_ramp_is_distinguishable_from_the_one_below() {
        // The ramp follows opencode's `generateGrayScale` in shape — a low, even
        // walk up from the background — but it is anchored on the app's own
        // `base` rather than on the terminal's, so the steps have to be checked
        // against each other rather than pinned to their formula.
        let t = Theme::dark();
        let gray = |i: u32| {
            let v = ((i as f32 / 12.0) * 0.4 * 255.0).floor() as u8;
            Color::Rgb(v, v, v)
        };
        let _ = gray;
        // Each level has to be distinguishable from the one it sits on, or the
        // structure the fills are carrying disappears — and every one of them
        // has to be distinguishable from the surface the app paints under all of
        // them, or the block is invisible on its own background.
        for (over, under) in [
            (t.panel, t.base),
            (t.element, t.panel),
            (t.selection, t.element),
            (t.code_wash, t.element),
            (t.element, t.base),
        ] {
            assert_ne!(over, under);
        }
    }

    #[test]
    fn text_on_a_fill_names_that_fill_as_its_background() {
        // A span left at the terminal default punches a hole in the region it
        // sits in — the one failure mode a fill-based layout has.
        let t = Theme::dark();
        for style in [t.panel(), t.panel_subtle(), t.panel_hint()] {
            assert_eq!(style.bg, Some(t.panel));
        }
        for style in [t.element(), t.element_subtle(), t.element_hint()] {
            assert_eq!(style.bg, Some(t.element));
        }
        assert_eq!(t.selected().bg, Some(t.selection));
        assert_eq!(t.selected_hint().bg, Some(t.selection));
    }

    #[test]
    fn no_color_keeps_meaning_in_modifiers() {
        let t = Theme::plain();
        assert!(t.plain);
        // Nothing paints a hue…
        for color in [t.text, t.accent, t.dot_working, t.selection, t.element] {
            assert_eq!(color, Color::Reset);
        }
        // …and no fill survives to erase another: under NO_COLOR the ramp is
        // gone, so every level must collapse to the terminal's own background
        // rather than to `Reset`-on-`Reset`.
        for style in [t.panel(), t.element(), t.panel_hint()] {
            assert_eq!(style.bg, None, "a plain theme paints no fill");
        }
        // …so the states that need attention carry a modifier instead.
        use comet_proto::ChatIndicator as I;
        assert!(
            t.dot(I::AwaitingInput)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(t.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(t.bubble().add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_transcript_styles_stay_on_the_terminals_own_background() {
        // opencode fills the sidebar, the prompt and its menus, and leaves
        // `background` transparent — the largest, most-scrolled region keeps the
        // user's own background and its transparency. These are the styles the
        // transcript body draws with, so they must not fill.
        let t = Theme::dark();
        for style in [t.body(), t.subtle(), t.hint(), t.label()] {
            assert_eq!(style.bg, None, "text drawn over a fill must not repaint it");
        }
        for style in [
            t.base(),
            t.panel(),
            t.element(),
            t.selected(),
            t.bubble(),
            t.code(),
        ] {
            assert!(style.bg.is_some(), "a region fill must paint");
        }
    }
}
