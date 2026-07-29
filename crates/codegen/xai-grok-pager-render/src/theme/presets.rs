//! Hyper preset theme collection.
//!
//! Fourteen additional built-in themes, layered on top of the original five
//! (GrokNight / GrokDay / TokyoNight / RosePineMoon / OscuraMidnight). They
//! are distinguished primarily by **background color** — eleven dark canvases
//! and three light ones — so users can pick a terminal mood at a glance.
//!
//! ## Why a shared builder
//!
//! The original themes each spell out all ~70 `Theme` fields by hand. That is
//! fine for five, but error-prone for a growing catalog. Instead, every preset supplies a
//! compact [`Palette`] of the ~18 colors that actually carry a theme's
//! identity, and [`build`] expands it into the full [`Theme`] with consistent
//! semantic-role assignments. This keeps the collection coherent (an error
//! accent always reads red, a code block always sinks to `surface`, …) and
//! makes adding another preset a one-palette change.
//!
//! All colors are `Color::Rgb` (truecolor). Like TokyoNight/RosePine/Oscura,
//! these presets are gated on truecolor support via
//! [`super::ThemeKind::requires_truecolor`] and fall back to GrokNight on
//! 256/16-color terminals.

use ratatui::style::{Color, Modifier};

use super::tokyonight::Theme;

/// Concise `const` RGB literal.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Blend `accent` into `base` by `pct` percent (0 = all base, 100 = all
/// accent). Used to derive polarity-correct diff bands from each palette's
/// canvas: a dark theme yields a dark-tinted band, a light theme a pale one,
/// with no per-theme hand-tuning. Non-RGB inputs pass `base` through.
const fn blend(base: Color, accent: Color, pct: u16) -> Color {
    match (base, accent) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(cr, cg, cb)) => {
            let q = 100 - pct;
            rgb(
                ((ar as u16 * q + cr as u16 * pct) / 100) as u8,
                ((ag as u16 * q + cg as u16 * pct) / 100) as u8,
                ((ab as u16 * q + cb as u16 * pct) / 100) as u8,
            )
        }
        _ => base,
    }
}

/// The minimal set of colors that define a preset's identity.
///
/// Backgrounds run dark→light: `base` is the main canvas, `surface` the sunken
/// tone (code blocks, scrollbar track, paste chips), `elevated` the raised row
/// tone (highlight/selection rows), `overlay` the hover/visual band. `border`
/// is the idle chrome frame; `border_active` the focused/selection frame.
/// Text runs `text` (primary) → `text_dim` → `muted` → `subtle` (dimmest).
/// `primary` is the signature accent (user turn, skills, fuzzy matches);
/// `secondary` the companion accent (assistant/thinking/verify). The six named
/// hues carry semantic roles (see [`build`]).
#[derive(Clone, Copy)]
struct Palette {
    base: Color,
    surface: Color,
    elevated: Color,
    overlay: Color,
    border: Color,
    border_active: Color,
    text: Color,
    text_dim: Color,
    muted: Color,
    subtle: Color,
    primary: Color,
    secondary: Color,
    red: Color,
    green: Color,
    yellow: Color,
    blue: Color,
    cyan: Color,
    orange: Color,
}

/// Expand a compact [`Palette`] into a full [`Theme`].
///
/// Field-to-role mapping mirrors the hand-authored `oscura_midnight` theme so
/// the presets stay visually consistent with the originals. The scrollbar
/// thumb is pinned to `muted` (a text-weight gray) against a `surface` track,
/// which guarantees the ≥30 summed-RGB contrast that
/// `scrollbar_thumb_contrasts_with_track_in_all_themes` enforces, in both
/// polarities. Diff bands are derived from the canvas via [`blend`].
const fn build(p: Palette) -> Theme {
    Theme {
        bg_base: p.base,
        bg_light: p.elevated,
        bg_dark: p.surface,
        bg_highlight: p.elevated,
        bg_hover: p.overlay,
        bg_terminal: p.base,

        accent_user: p.primary,
        accent_assistant: p.secondary,
        accent_thinking: p.muted,
        accent_tool: p.subtle,
        accent_system: p.blue,
        accent_error: p.red,
        accent_success: p.green,
        accent_running: p.secondary,
        accent_skill: p.primary,

        text_primary: p.text,
        text_secondary: p.text_dim,

        gray_dim: p.subtle,
        gray: p.muted,
        gray_bright: p.text_dim,

        command: p.yellow,
        path: p.orange,
        running: p.cyan,
        warning: p.yellow,

        fuzzy_accent: p.primary,

        accent_plan: p.yellow,

        accent_verify: p.secondary,

        accent_feedback: p.cyan,

        accent_remember: p.green,

        selection_border: p.border_active,
        hover_border: p.border,
        prompt_border: p.border,
        prompt_border_active: p.border_active,

        accent_model: p.cyan,

        // Track = sunken surface, thumb = muted text-gray. On dark themes
        // `muted` sits far lighter than `surface`; on light themes far darker.
        // Either way the delta clears the ≥30 contrast floor with room for the
        // 40% follow-mode blend toward the track.
        scrollbar_bg: p.surface,
        scrollbar_fg: p.muted,

        diff_delete_bg: blend(p.base, p.red, 22),
        diff_delete_fg: p.red,
        diff_insert_bg: blend(p.base, p.green, 22),
        diff_insert_fg: p.green,
        diff_equal_fg: p.muted,
        diff_gutter_fg: p.subtle,

        bg_visual: p.overlay,

        paste_bg: p.surface,
        paste_fg: p.text_dim,
        paste_dim: p.muted,

        md_heading_h1: p.text,
        md_heading_h1_mod: Modifier::BOLD,
        md_heading_h2: p.primary,
        md_heading_h2_mod: Modifier::BOLD,
        md_heading_h3: p.secondary,
        md_heading_h3_mod: Modifier::BOLD,
        md_heading_h4: p.cyan,
        md_heading_h4_mod: Modifier::BOLD.union(Modifier::ITALIC),
        md_heading_h5: p.yellow,
        md_heading_h5_mod: Modifier::BOLD,
        md_heading_h6: p.blue,
        md_heading_h6_mod: Modifier::BOLD,
        md_code: p.cyan,
        md_task_checked: p.green,
        md_task_unchecked: p.text_dim,
        md_muted: p.muted,
        md_code_bg: p.surface,
        md_text: p.text,
        link_fg: p.blue,
    }
}

// ===========================================================================
// Dark presets (11)
// ===========================================================================

/// Everforest — soft, low-contrast forest greens.
const EVERFOREST: Palette = Palette {
    base: rgb(43, 51, 57),
    surface: rgb(35, 42, 46),
    elevated: rgb(55, 65, 69),
    overlay: rgb(74, 85, 91),
    border: rgb(74, 85, 91),
    border_active: rgb(122, 132, 120),
    text: rgb(211, 198, 170),
    text_dim: rgb(157, 169, 160),
    muted: rgb(133, 146, 137),
    subtle: rgb(79, 88, 94),
    primary: rgb(167, 192, 128),
    secondary: rgb(214, 153, 182),
    red: rgb(230, 126, 128),
    green: rgb(167, 192, 128),
    yellow: rgb(219, 188, 127),
    blue: rgb(127, 187, 179),
    cyan: rgb(131, 192, 146),
    orange: rgb(230, 152, 117),
};

/// Nord — cold arctic slate with frost accents.
const NORD: Palette = Palette {
    base: rgb(46, 52, 64),
    surface: rgb(39, 43, 53),
    elevated: rgb(59, 66, 82),
    overlay: rgb(67, 76, 94),
    border: rgb(67, 76, 94),
    border_active: rgb(129, 161, 193),
    text: rgb(229, 233, 240),
    text_dim: rgb(216, 222, 233),
    muted: rgb(123, 136, 161),
    subtle: rgb(76, 86, 106),
    primary: rgb(136, 192, 208),
    secondary: rgb(180, 142, 173),
    red: rgb(191, 97, 106),
    green: rgb(163, 190, 140),
    yellow: rgb(235, 203, 139),
    blue: rgb(129, 161, 193),
    cyan: rgb(143, 188, 187),
    orange: rgb(208, 135, 112),
};

/// Dracula — purple-charcoal canvas, vivid pink/purple accents.
const DRACULA: Palette = Palette {
    base: rgb(40, 42, 54),
    surface: rgb(33, 34, 44),
    elevated: rgb(52, 55, 70),
    overlay: rgb(68, 71, 90),
    border: rgb(68, 71, 90),
    border_active: rgb(98, 114, 164),
    text: rgb(248, 248, 242),
    text_dim: rgb(200, 200, 216),
    muted: rgb(139, 143, 168),
    subtle: rgb(86, 88, 114),
    primary: rgb(189, 147, 249),
    secondary: rgb(255, 121, 198),
    red: rgb(255, 85, 85),
    green: rgb(80, 250, 123),
    yellow: rgb(241, 250, 140),
    blue: rgb(139, 233, 253),
    cyan: rgb(139, 233, 253),
    orange: rgb(255, 184, 108),
};

/// Gruvbox — warm retro browns with high-chroma accents.
const GRUVBOX: Palette = Palette {
    base: rgb(40, 40, 40),
    surface: rgb(29, 32, 33),
    elevated: rgb(60, 56, 54),
    overlay: rgb(80, 73, 69),
    border: rgb(80, 73, 69),
    border_active: rgb(102, 92, 84),
    text: rgb(235, 219, 178),
    text_dim: rgb(213, 196, 161),
    muted: rgb(168, 153, 132),
    subtle: rgb(124, 111, 100),
    primary: rgb(250, 189, 47),
    secondary: rgb(211, 134, 155),
    red: rgb(251, 73, 52),
    green: rgb(184, 187, 38),
    yellow: rgb(250, 189, 47),
    blue: rgb(131, 165, 152),
    cyan: rgb(142, 192, 124),
    orange: rgb(254, 128, 25),
};

/// Catppuccin Mocha — muted pastel dark with a mauve signature.
const CATPPUCCIN_MOCHA: Palette = Palette {
    base: rgb(30, 30, 46),
    surface: rgb(24, 24, 37),
    elevated: rgb(49, 50, 68),
    overlay: rgb(69, 71, 90),
    border: rgb(69, 71, 90),
    border_active: rgb(108, 112, 134),
    text: rgb(205, 214, 244),
    text_dim: rgb(186, 194, 222),
    muted: rgb(166, 173, 200),
    subtle: rgb(88, 91, 112),
    primary: rgb(203, 166, 247),
    secondary: rgb(245, 194, 231),
    red: rgb(243, 139, 168),
    green: rgb(166, 227, 161),
    yellow: rgb(249, 226, 175),
    blue: rgb(137, 180, 250),
    cyan: rgb(148, 226, 213),
    orange: rgb(250, 179, 135),
};

/// Solarized Dark — precision-tuned teal base, classic Solarized accents.
const SOLARIZED_DARK: Palette = Palette {
    base: rgb(0, 43, 54),
    surface: rgb(7, 54, 66),
    elevated: rgb(10, 67, 81),
    overlay: rgb(16, 82, 98),
    border: rgb(10, 67, 81),
    border_active: rgb(88, 110, 117),
    text: rgb(147, 161, 161),
    text_dim: rgb(131, 148, 150),
    muted: rgb(101, 123, 131),
    subtle: rgb(88, 110, 117),
    primary: rgb(38, 139, 210),
    secondary: rgb(108, 113, 196),
    red: rgb(220, 50, 47),
    green: rgb(133, 153, 0),
    yellow: rgb(181, 137, 0),
    blue: rgb(38, 139, 210),
    cyan: rgb(42, 161, 152),
    orange: rgb(203, 75, 22),
};

/// Deep Ocean — near-black navy with luminous blues.
const DEEP_OCEAN: Palette = Palette {
    base: rgb(15, 17, 26),
    surface: rgb(9, 11, 16),
    elevated: rgb(31, 34, 51),
    overlay: rgb(42, 47, 69),
    border: rgb(42, 47, 69),
    border_active: rgb(70, 75, 93),
    text: rgb(166, 172, 205),
    text_dim: rgb(143, 150, 179),
    muted: rgb(113, 124, 180),
    subtle: rgb(75, 82, 109),
    primary: rgb(130, 170, 255),
    secondary: rgb(199, 146, 234),
    red: rgb(240, 113, 120),
    green: rgb(195, 232, 141),
    yellow: rgb(255, 203, 107),
    blue: rgb(130, 170, 255),
    cyan: rgb(137, 221, 255),
    orange: rgb(247, 140, 108),
};

/// Ember — dark maroon canvas, warm rose/amber accents.
const EMBER: Palette = Palette {
    base: rgb(35, 22, 26),
    surface: rgb(26, 16, 19),
    elevated: rgb(51, 36, 42),
    overlay: rgb(69, 49, 58),
    border: rgb(69, 49, 58),
    border_active: rgb(110, 74, 83),
    text: rgb(240, 217, 213),
    text_dim: rgb(211, 176, 170),
    muted: rgb(176, 138, 134),
    subtle: rgb(122, 90, 94),
    primary: rgb(255, 143, 112),
    secondary: rgb(224, 96, 126),
    red: rgb(255, 107, 107),
    green: rgb(181, 206, 168),
    yellow: rgb(255, 204, 102),
    blue: rgb(143, 184, 201),
    cyan: rgb(127, 201, 193),
    orange: rgb(255, 159, 107),
};

/// Base16 Default Dark — the canonical Base16 dark palette by Chris Kempson.
///
/// The neutral slots follow the Base16 styling guide directly: `base00` is
/// the canvas, `base01` is the lighter/status surface, `base02` is selection,
/// `base03` is muted chrome/comments, and `base04`/`base05` are secondary and
/// primary foregrounds. The companion TextMate theme carries all sixteen
/// slots, including the infrequently used `base06`, `base07`, and `base0F`.
const BASE16_DEFAULT_DARK: Palette = Palette {
    base: rgb(0x18, 0x18, 0x18),          // base00
    surface: rgb(0x28, 0x28, 0x28),       // base01
    elevated: rgb(0x28, 0x28, 0x28),      // base01
    overlay: rgb(0x38, 0x38, 0x38),       // base02
    border: rgb(0x58, 0x58, 0x58),        // base03
    border_active: rgb(0xb8, 0xb8, 0xb8), // base04
    text: rgb(0xd8, 0xd8, 0xd8),          // base05
    text_dim: rgb(0xb8, 0xb8, 0xb8),      // base04
    muted: rgb(0x58, 0x58, 0x58),         // base03
    subtle: rgb(0x58, 0x58, 0x58),        // base03
    primary: rgb(0x7c, 0xaf, 0xc2),       // base0D
    secondary: rgb(0xba, 0x8b, 0xaf),     // base0E
    red: rgb(0xab, 0x46, 0x42),           // base08
    green: rgb(0xa1, 0xb5, 0x6c),         // base0B
    yellow: rgb(0xf7, 0xca, 0x88),        // base0A
    blue: rgb(0x7c, 0xaf, 0xc2),          // base0D
    cyan: rgb(0x86, 0xc1, 0xb9),          // base0C
    orange: rgb(0xdc, 0x96, 0x56),        // base09
};

/// OMP Titanium — high-contrast titanium surfaces with electric-blue accents.
///
/// Mirrors Oh My Pi's default dark theme: terminal-native bright text on a
/// near-black blue-gray canvas, one strong cyan-blue navigation accent, and
/// vivid green/red/amber semantic states.
const OMP: Palette = Palette {
    base: rgb(21, 24, 32),
    surface: rgb(15, 18, 22),
    elevated: rgb(31, 37, 45),
    overlay: rgb(42, 48, 56),
    border: rgb(42, 48, 56),
    border_active: rgb(0, 180, 255),
    text: rgb(232, 236, 244),
    text_dim: rgb(156, 163, 176),
    muted: rgb(107, 114, 128),
    subtle: rgb(74, 80, 88),
    primary: rgb(0, 180, 255),
    secondary: rgb(212, 192, 144),
    red: rgb(255, 71, 87),
    green: rgb(0, 255, 136),
    yellow: rgb(255, 179, 71),
    blue: rgb(0, 180, 255),
    cyan: rgb(0, 180, 255),
    orange: rgb(212, 192, 144),
};

/// Midnight OLED — pure black for OLED panels, amber-forward accents.
const MIDNIGHT_OLED: Palette = Palette {
    base: rgb(0, 0, 0),
    surface: rgb(0, 0, 0),
    elevated: rgb(18, 18, 18),
    overlay: rgb(30, 30, 30),
    border: rgb(30, 30, 30),
    border_active: rgb(58, 58, 58),
    text: rgb(232, 232, 232),
    text_dim: rgb(192, 192, 192),
    muted: rgb(138, 138, 138),
    subtle: rgb(74, 74, 74),
    primary: rgb(255, 176, 0),
    secondary: rgb(255, 123, 0),
    red: rgb(255, 92, 87),
    green: rgb(90, 247, 142),
    yellow: rgb(255, 176, 0),
    blue: rgb(87, 199, 255),
    cyan: rgb(154, 237, 254),
    orange: rgb(255, 159, 67),
};

// ===========================================================================
// Light presets (3)
// ===========================================================================

/// Solarized Light — warm cream canvas, classic Solarized accents.
const SOLARIZED_LIGHT: Palette = Palette {
    base: rgb(253, 246, 227),
    surface: rgb(238, 232, 213),
    elevated: rgb(228, 221, 200),
    overlay: rgb(217, 210, 189),
    border: rgb(217, 210, 189),
    border_active: rgb(147, 161, 161),
    text: rgb(88, 110, 117),
    text_dim: rgb(101, 123, 131),
    muted: rgb(131, 148, 150),
    subtle: rgb(147, 161, 161),
    primary: rgb(38, 139, 210),
    secondary: rgb(108, 113, 196),
    red: rgb(220, 50, 47),
    green: rgb(133, 153, 0),
    yellow: rgb(181, 137, 0),
    blue: rgb(38, 139, 210),
    cyan: rgb(42, 161, 152),
    orange: rgb(203, 75, 22),
};

/// Catppuccin Latte — cool light gray-blue with saturated accents.
const CATPPUCCIN_LATTE: Palette = Palette {
    base: rgb(239, 241, 245),
    surface: rgb(230, 233, 239),
    elevated: rgb(220, 224, 232),
    overlay: rgb(204, 208, 218),
    border: rgb(204, 208, 218),
    border_active: rgb(124, 127, 147),
    text: rgb(76, 79, 105),
    text_dim: rgb(92, 95, 119),
    muted: rgb(108, 111, 133),
    subtle: rgb(140, 143, 161),
    primary: rgb(136, 57, 239),
    secondary: rgb(234, 118, 203),
    red: rgb(210, 15, 57),
    green: rgb(64, 160, 43),
    yellow: rgb(223, 142, 29),
    blue: rgb(30, 102, 245),
    cyan: rgb(23, 146, 153),
    orange: rgb(254, 100, 11),
};

/// Paper — warm sepia canvas evoking printed paper, muted earth accents.
const PAPER: Palette = Palette {
    base: rgb(244, 236, 216),
    surface: rgb(235, 226, 200),
    elevated: rgb(224, 214, 184),
    overlay: rgb(211, 199, 163),
    border: rgb(211, 199, 163),
    border_active: rgb(168, 153, 104),
    text: rgb(74, 64, 50),
    text_dim: rgb(95, 84, 66),
    muted: rgb(122, 111, 88),
    subtle: rgb(156, 143, 112),
    primary: rgb(143, 94, 60),
    secondary: rgb(160, 62, 94),
    red: rgb(181, 66, 58),
    green: rgb(107, 125, 58),
    yellow: rgb(181, 133, 26),
    blue: rgb(58, 107, 143),
    cyan: rgb(58, 143, 125),
    orange: rgb(201, 106, 43),
};

impl Theme {
    /// Everforest — soft low-contrast forest greens (dark).
    pub const fn everforest() -> Self {
        build(EVERFOREST)
    }

    /// Nord — cold arctic slate with frost accents (dark).
    pub const fn nord() -> Self {
        build(NORD)
    }

    /// Dracula — purple-charcoal with vivid pink/purple accents (dark).
    pub const fn dracula() -> Self {
        build(DRACULA)
    }

    /// Gruvbox — warm retro browns with high-chroma accents (dark).
    pub const fn gruvbox() -> Self {
        build(GRUVBOX)
    }

    /// Catppuccin Mocha — muted pastel dark with a mauve signature (dark).
    pub const fn catppuccin_mocha() -> Self {
        build(CATPPUCCIN_MOCHA)
    }

    /// Solarized Dark — precision-tuned teal base (dark).
    pub const fn solarized_dark() -> Self {
        build(SOLARIZED_DARK)
    }

    /// Deep Ocean — near-black navy with luminous blues (dark).
    pub const fn deep_ocean() -> Self {
        build(DEEP_OCEAN)
    }

    /// Ember — dark maroon with warm rose/amber accents (dark).
    pub const fn ember() -> Self {
        build(EMBER)
    }

    /// Midnight OLED — pure black with amber-forward accents (dark).
    pub const fn midnight_oled() -> Self {
        build(MIDNIGHT_OLED)
    }

    /// Base16 Default Dark — canonical Base16 palette by Chris Kempson.
    pub const fn base16_default_dark() -> Self {
        build(BASE16_DEFAULT_DARK)
    }

    /// OMP Titanium — high-contrast blue-gray surfaces with electric-blue accents.
    pub const fn omp() -> Self {
        build(OMP)
    }

    /// Solarized Light — warm cream canvas (light).
    pub const fn solarized_light() -> Self {
        build(SOLARIZED_LIGHT)
    }

    /// Catppuccin Latte — cool light gray-blue (light).
    pub const fn catppuccin_latte() -> Self {
        build(CATPPUCCIN_LATTE)
    }

    /// Paper — warm sepia printed-paper canvas (light).
    pub const fn paper() -> Self {
        build(PAPER)
    }
}
