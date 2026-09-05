use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(not(test))]
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    Minimal,
    Soft,
    Dark,
    Gray,
    SoftPink,
    Midnight,
}

impl ThemeKind {
    // Order is the picker order *and* the on-disk index, so the original three
    // keep their positions and the DevezCode-parity themes append after them.
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Soft,
        Self::Dark,
        Self::Gray,
        Self::SoftPink,
        Self::Midnight,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" | "min" | "light" => Some(Self::Minimal),
            "soft" | "warm" => Some(Self::Soft),
            "dark" => Some(Self::Dark),
            "gray" | "grey" => Some(Self::Gray),
            "softpink" | "soft-pink" | "pink" => Some(Self::SoftPink),
            "midnight" | "midnightblue" | "midnight-blue" => Some(Self::Midnight),
            _ => None,
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Soft => "soft",
            Self::Dark => "dark",
            Self::Gray => "gray",
            Self::SoftPink => "softpink",
            Self::Midnight => "midnight",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Soft => "Soft",
            Self::Dark => "Dark",
            Self::Gray => "Gray",
            Self::SoftPink => "Soft Pink",
            Self::Midnight => "Midnight Blue",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Minimal => "cool white · blue accent",
            Self::Soft => "warm cream · green accent",
            Self::Dark => "charcoal · orange accent",
            Self::Gray => "neutral gray · slate accent",
            Self::SoftPink => "warm blush · rose accent",
            Self::Midnight => "deep navy · azure accent",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

/// Drag-selection background for the active theme. Dark carries Claude Code's
/// own dark-theme `selectionBg` (`rgb(38, 79, 120)`); the light themes take the
/// `selectionBg` DevezCode overrides it with per theme
/// (`Services/Terminal/ClaudeCustomThemes.cs`). So a drag here looks like a drag
/// there — a tinted wash of each theme's accent instead of one charcoal block
/// that only ever suited Dark.
pub fn selection_bg() -> Rgb {
    palette().selection_bg
}

/// Used only for text the theme's own block would swallow. Syntax colours keep
/// themselves inside the selection wherever they stay readable; the runs that
/// don't — and only those — fall back to this.
pub fn selection_fg() -> Rgb {
    palette().selection_fg
}

/// Foreground to paint `tone_color` in when it sits inside a selection block.
pub fn selection_text(color: Rgb) -> Rgb {
    if contrast_ratio(color, selection_bg()) >= 4.5 {
        color
    } else {
        selection_fg()
    }
}

pub fn contrast_ratio(left: Rgb, right: Rgb) -> f64 {
    let light = relative_luminance(left).max(relative_luminance(right));
    let dark = relative_luminance(left).min(relative_luminance(right));
    (light + 0.05) / (dark + 0.05)
}

fn relative_luminance(color: Rgb) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.0) + 0.7152 * channel(color.1) + 0.0722 * channel(color.2)
}

#[derive(Clone, Copy)]
pub struct ThemePalette {
    pub background: Rgb,
    pub foreground: Rgb,
    pub border: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub blue: Rgb,
    /// Desaturated sky blue reserved for the GPT-5.5 model family.
    #[allow(dead_code)]
    pub sky_blue: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
    pub error: Rgb,
    #[allow(dead_code)]
    pub purple: Rgb,
    #[allow(dead_code)]
    pub pink: Rgb,
    #[allow(dead_code)]
    pub orange: Rgb,
    /// Semantic colors for Markdown responses. Keep structure, navigation, and
    /// inline code visually distinct instead of tinting every emphasized token
    /// with the theme accent.
    pub response: ResponsePalette,
    pub model_gpt56: Rgb,
    pub model_gpt55: Rgb,
    /// OpenCode chrome and status text. Uses each theme's default provider
    /// blue so the model token matches the base palette.
    pub model_opencode: Rgb,
    pub model_astra: Rgb,
    pub model_sol: Rgb,
    pub model_terra: Rgb,
    pub model_luna: Rgb,
    pub model_spark: Rgb,
    /// The agent roles. Each theme carries its own four so a role reads the
    /// same weight on a light background as on a dark one instead of borrowing
    /// a colour that happens to sit nearby in the palette.
    pub agent_builder: Rgb,
    pub agent_planner: Rgb,
    pub agent_goal_runner: Rgb,
    pub agent_reviewer: Rgb,
    pub status: StatusLinePalette,
    pub code: Rgb,
    pub syntax_comment: Rgb,
    pub syntax_string: Rgb,
    pub syntax_keyword: Rgb,
    pub syntax_number: Rgb,
    pub syntax_type: Rgb,
    pub syntax_function: Rgb,
    pub syntax_attribute: Rgb,
    pub syntax_property: Rgb,
    pub diff_add_bg: Rgb,
    pub diff_remove_bg: Rgb,
    /// The stronger tint painted over just the words that changed inside an
    /// added/removed row, the way Claude Code's `diffAddedWord` does.
    pub diff_add_word_bg: Rgb,
    pub diff_remove_word_bg: Rgb,
    pub user_prompt_bg: Rgb,
    pub model_change_bg: Rgb,
    pub hover_bg: Rgb,
    /// Drag-selection block. See `selection_bg`.
    pub selection_bg: Rgb,
    /// Fallback text colour inside that block. See `selection_fg`.
    pub selection_fg: Rgb,
}

#[derive(Clone, Copy)]
pub struct ResponsePalette {
    /// Section titles and primary structure.
    pub heading: Rgb,
    /// Clickable Markdown links.
    pub link: Rgb,
    /// Short technical identifiers surrounded by backticks.
    pub inline_code: Rgb,
}

/// Status line colors, kept apart from the rest of the palette because they are
/// copied verbatim from DevezCode's `Resources/StatusLine/statusline.js` so both
/// products paint the same row. That script maximizes saturation on light
/// backgrounds to keep the segments tellable apart, which trades away the
/// background contrast the other palette colors are held to — see
/// `status_line_colors_stay_visible`. Do not "fix" these values in isolation;
/// change them together with the shared script or the two drift apart.
#[derive(Clone, Copy)]
pub struct StatusLinePalette {
    /// Fallback for segments with no color of their own (`MAIN`).
    pub text: Rgb,
    /// The ` | ` between segments (`SEP`).
    pub separator: Rgb,
    pub branch: Rgb,
    /// The `ctx:` segment (`CTX`).
    #[allow(dead_code)]
    pub context: Rgb,
    pub model_haiku: Rgb,
    pub model_sonnet: Rgb,
    pub model_opus: Rgb,
    pub model_fable: Rgb,
    pub effort_low: Rgb,
    pub effort_medium: Rgb,
    pub effort_high: Rgb,
    pub effort_xhigh: Rgb,
    pub effort_max: Rgb,
    /// The tier past `max`. Carries on the ramp's hue walk into magenta so it
    /// reads as "beyond max" rather than as a second red.
    pub effort_ultra: Rgb,
    /// The 5h window segment (`TIME`).
    pub five_hour: Rgb,
    /// The `week:` segment (`WEEK`).
    pub weekly: Rgb,
}

pub const MINIMAL: ThemePalette = ThemePalette {
    background: Rgb(0xF8, 0xFA, 0xFC),
    foreground: Rgb(0x0F, 0x17, 0x2A),
    border: Rgb(0x94, 0xA3, 0xB8),
    muted: Rgb(0x47, 0x55, 0x69),
    accent: Rgb(0x25, 0x63, 0xEB),
    blue: Rgb(0x25, 0x63, 0xEB),
    sky_blue: Rgb(0x34, 0x76, 0x9E),
    success: Rgb(0x15, 0x80, 0x3D),
    warning: Rgb(0x9A, 0x67, 0x00),
    error: Rgb(0xDC, 0x26, 0x26),
    purple: Rgb(0x7C, 0x3A, 0xED),
    pink: Rgb(0xBE, 0x18, 0x5D),
    orange: Rgb(0xBC, 0x4C, 0x00),
    response: ResponsePalette {
        heading: Rgb(0x25, 0x63, 0xEB),
        link: Rgb(0x34, 0x76, 0x9E),
        inline_code: Rgb(0x7C, 0x3A, 0xED),
    },
    model_gpt56: Rgb(0x25, 0x63, 0xEB),
    model_gpt55: Rgb(0x25, 0x63, 0xEB),
    model_opencode: Rgb(0x25, 0x63, 0xEB),
    model_astra: Rgb(0xD1, 0x00, 0x00),
    model_sol: Rgb(0xFF, 0x6B, 0x00),
    model_terra: Rgb(0x2E, 0x7D, 0x32),
    model_luna: Rgb(0x6C, 0x5C, 0xE7),
    model_spark: Rgb(0xEA, 0xB3, 0x08),
    agent_builder: Rgb(0x34, 0x76, 0x9E),
    agent_planner: Rgb(0x15, 0x80, 0x3D),
    agent_goal_runner: Rgb(0xDC, 0x26, 0x26),
    agent_reviewer: Rgb(0x7C, 0x3A, 0xED),
    status: StatusLinePalette {
        text: Rgb(0x0F, 0x14, 0x22),
        separator: Rgb(0x0F, 0x14, 0x22),
        branch: Rgb(0x00, 0x91, 0xD1),
        context: Rgb(0x00, 0x8E, 0x5A),
        model_haiku: Rgb(0x00, 0xD1, 0xD1),
        model_sonnet: Rgb(0xCA, 0x8A, 0x04),
        model_opus: Rgb(0xD1, 0x00, 0x00),
        model_fable: Rgb(0xB5, 0x00, 0xD1),
        effort_low: Rgb(0xD1, 0x9F, 0x00),
        effort_medium: Rgb(0x00, 0xA7, 0x40),
        effort_high: Rgb(0x43, 0x38, 0xCA),
        effort_xhigh: Rgb(0x7C, 0x00, 0xD1),
        effort_max: Rgb(0xD1, 0x00, 0x00),
        effort_ultra: Rgb(0xC2, 0x00, 0x78),
        five_hour: Rgb(0x00, 0x5E, 0xD1),
        weekly: Rgb(0x35, 0x00, 0xD1),
    },
    code: Rgb(0x0F, 0x17, 0x2A),
    syntax_comment: Rgb(0x00, 0x80, 0x00),
    syntax_string: Rgb(0xA3, 0x15, 0x15),
    syntax_keyword: Rgb(0x00, 0x00, 0xFF),
    syntax_number: Rgb(0x05, 0x7A, 0x55),
    syntax_type: Rgb(0x1F, 0x71, 0x87),
    syntax_function: Rgb(0x79, 0x5E, 0x26),
    syntax_attribute: Rgb(0xA3, 0x15, 0x15),
    syntax_property: Rgb(0x00, 0x10, 0x80),
    diff_add_bg: Rgb(0xDA, 0xF0, 0xDE),
    diff_remove_bg: Rgb(0xF1, 0xD7, 0xDA),
    diff_add_word_bg: Rgb(0x9E, 0xDD, 0xAE),
    diff_remove_word_bg: Rgb(0xF0, 0xAA, 0xB2),
    user_prompt_bg: Rgb(0xEE, 0xF4, 0xFF),
    model_change_bg: Rgb(0xDB, 0xEA, 0xFE),
    hover_bg: Rgb(0xE8, 0xEE, 0xF7),
    selection_bg: Rgb(0xC5, 0xD8, 0xF8),
    selection_fg: Rgb(0x0F, 0x17, 0x2A),
};

pub const SOFT: ThemePalette = ThemePalette {
    background: Rgb(0xF2, 0xED, 0xE6),
    foreground: Rgb(0x2A, 0x26, 0x20),
    border: Rgb(0x9A, 0x91, 0x7F),
    muted: Rgb(0x5A, 0x54, 0x48),
    accent: Rgb(0x42, 0x68, 0x34),
    blue: Rgb(0x42, 0x63, 0x8F),
    sky_blue: Rgb(0x3D, 0x6E, 0x8E),
    success: Rgb(0x16, 0x65, 0x34),
    warning: Rgb(0x8A, 0x4B, 0x08),
    error: Rgb(0xA3, 0x3E, 0x3E),
    purple: Rgb(0x68, 0x4B, 0x8A),
    pink: Rgb(0x8F, 0x3D, 0x5A),
    orange: Rgb(0x9A, 0x4D, 0x12),
    response: ResponsePalette {
        heading: Rgb(0x42, 0x63, 0x8F),
        link: Rgb(0x3D, 0x6E, 0x8E),
        inline_code: Rgb(0x68, 0x4B, 0x8A),
    },
    model_gpt56: Rgb(0x25, 0x63, 0xEB),
    model_gpt55: Rgb(0x4A, 0x69, 0x84),
    model_opencode: Rgb(0x42, 0x63, 0x8F),
    model_astra: Rgb(0xCC, 0x00, 0x00),
    model_sol: Rgb(0xD9, 0x77, 0x06),
    model_terra: Rgb(0x55, 0x7A, 0x46),
    model_luna: Rgb(0x8E, 0x7A, 0xB5),
    model_spark: Rgb(0xC2, 0x9B, 0x38),
    agent_builder: Rgb(0x3D, 0x6E, 0x8E),
    agent_planner: Rgb(0x16, 0x65, 0x34),
    agent_goal_runner: Rgb(0xA3, 0x3E, 0x3E),
    agent_reviewer: Rgb(0x68, 0x4B, 0x8A),
    status: StatusLinePalette {
        text: Rgb(0x16, 0x12, 0x0C),
        separator: Rgb(0x16, 0x12, 0x0C),
        branch: Rgb(0x00, 0x8D, 0xCC),
        context: Rgb(0x00, 0x8B, 0x58),
        model_haiku: Rgb(0x00, 0xCC, 0xCC),
        model_sonnet: Rgb(0xC9, 0x7C, 0x1A),
        model_opus: Rgb(0xCC, 0x00, 0x00),
        model_fable: Rgb(0xB1, 0x00, 0xCC),
        effort_low: Rgb(0xCC, 0x9C, 0x00),
        effort_medium: Rgb(0x00, 0xA3, 0x3F),
        effort_high: Rgb(0x43, 0x38, 0xCA),
        effort_xhigh: Rgb(0x79, 0x00, 0xCC),
        effort_max: Rgb(0xCC, 0x00, 0x00),
        effort_ultra: Rgb(0xBD, 0x00, 0x74),
        five_hour: Rgb(0x00, 0x5B, 0xCC),
        weekly: Rgb(0x33, 0x00, 0xCC),
    },
    code: Rgb(0x2A, 0x26, 0x20),
    syntax_comment: Rgb(0x32, 0x6A, 0x32),
    syntax_string: Rgb(0x98, 0x3B, 0x34),
    syntax_keyword: Rgb(0x33, 0x55, 0xCC),
    syntax_number: Rgb(0x2E, 0x6A, 0x4D),
    syntax_type: Rgb(0x42, 0x68, 0x34),
    syntax_function: Rgb(0x68, 0x4B, 0x8A),
    syntax_attribute: Rgb(0x9A, 0x4D, 0x12),
    syntax_property: Rgb(0x3D, 0x6E, 0x8E),
    diff_add_bg: Rgb(0xD9, 0xE5, 0xD8),
    diff_remove_bg: Rgb(0xEC, 0xD1, 0xD2),
    diff_add_word_bg: Rgb(0x7B, 0xAA, 0x68),
    diff_remove_word_bg: Rgb(0xE8, 0xA0, 0xA0),
    user_prompt_bg: Rgb(0xE6, 0xF0, 0xDE),
    model_change_bg: Rgb(0xDE, 0xEC, 0xD6),
    hover_bg: Rgb(0xE7, 0xE0, 0xD7),
    selection_bg: Rgb(0xC2, 0xD8, 0xB0),
    selection_fg: Rgb(0x2A, 0x26, 0x20),
};

pub const DARK: ThemePalette = ThemePalette {
    background: Rgb(0x1F, 0x1F, 0x1E),
    foreground: Rgb(0xE8, 0xE8, 0xE8),
    border: Rgb(0x77, 0x77, 0x77),
    muted: Rgb(0xAA, 0xAA, 0xAA),
    accent: Rgb(0xFF, 0xA5, 0x58),
    blue: Rgb(0x60, 0xA5, 0xFA),
    sky_blue: Rgb(0x78, 0xB2, 0xD2),
    success: Rgb(0x22, 0xC5, 0x5E),
    warning: Rgb(0xFB, 0xBF, 0x24),
    error: Rgb(0xF8, 0x71, 0x71),
    purple: Rgb(0xA7, 0x8B, 0xFA),
    pink: Rgb(0xF4, 0x72, 0xB6),
    orange: Rgb(0xFF, 0xA5, 0x58),
    response: ResponsePalette {
        heading: Rgb(0x78, 0xB2, 0xD2),
        link: Rgb(0x60, 0xA5, 0xFA),
        inline_code: Rgb(0xFF, 0xA5, 0x58),
    },
    model_gpt56: Rgb(0x00, 0xF0, 0xFF),
    model_gpt55: Rgb(0x60, 0xA5, 0xFA),
    model_opencode: Rgb(0x60, 0xA5, 0xFA),
    model_astra: Rgb(0xD7, 0x65, 0x64),
    model_sol: Rgb(0xFF, 0x9E, 0x59),
    model_terra: Rgb(0x81, 0xC7, 0x84),
    model_luna: Rgb(0xC4, 0xB5, 0xFD),
    model_spark: Rgb(0xFD, 0xE0, 0x47),
    agent_builder: Rgb(0x38, 0xBD, 0xF8),
    agent_planner: Rgb(0x5F, 0xBF, 0x7A),
    agent_goal_runner: Rgb(0xF8, 0x71, 0x71),
    agent_reviewer: Rgb(0xA7, 0x8B, 0xFA),
    status: StatusLinePalette {
        text: Rgb(0xC7, 0xC8, 0xCB),
        separator: Rgb(0x82, 0x90, 0xA0),
        branch: Rgb(0x82, 0xAC, 0xDA),
        context: Rgb(0x32, 0xB7, 0x86),
        model_haiku: Rgb(0x07, 0xDC, 0xDB),
        model_sonnet: Rgb(0xD9, 0xB1, 0x17),
        model_opus: Rgb(0xD7, 0x65, 0x64),
        model_fable: Rgb(0xCA, 0x6C, 0xD6),
        effort_low: Rgb(0xC0, 0x97, 0x14),
        effort_medium: Rgb(0x3C, 0x8A, 0x58),
        effort_high: Rgb(0x9B, 0xA1, 0xD6),
        effort_xhigh: Rgb(0x9A, 0x77, 0xDB),
        effort_max: Rgb(0xD7, 0x65, 0x64),
        effort_ultra: Rgb(0xDD, 0x7F, 0xB8),
        five_hour: Rgb(0x57, 0x91, 0xD7),
        weekly: Rgb(0x93, 0x7B, 0xD7),
    },
    code: Rgb(0xE8, 0xE8, 0xE8),
    syntax_comment: Rgb(0x6A, 0x99, 0x55),
    syntax_string: Rgb(0xCE, 0x91, 0x78),
    syntax_keyword: Rgb(0x56, 0x9C, 0xD6),
    syntax_number: Rgb(0xB5, 0xCE, 0xA8),
    syntax_type: Rgb(0x4E, 0xC9, 0xB0),
    syntax_function: Rgb(0xDC, 0xDC, 0xAA),
    syntax_attribute: Rgb(0xC5, 0x86, 0xC0),
    syntax_property: Rgb(0x9C, 0xDC, 0xFE),
    diff_add_bg: Rgb(0x26, 0x3D, 0x2A),
    diff_remove_bg: Rgb(0x47, 0x29, 0x28),
    diff_add_word_bg: Rgb(0x2A, 0x6B, 0x3C),
    diff_remove_word_bg: Rgb(0x8F, 0x3B, 0x38),
    user_prompt_bg: Rgb(0x36, 0x36, 0x36),
    model_change_bg: Rgb(0x43, 0x43, 0x43),
    hover_bg: Rgb(0x32, 0x32, 0x31),
    selection_bg: Rgb(0x26, 0x4F, 0x78),
    selection_fg: Rgb(0xE6, 0xE6, 0xE6),
};

/// DevezCode's `gray` theme (`App.xaml.cs` + `devez-gray.json`): neutral page,
/// slate accent, no hue of its own. Syntax keeps Minimal's light VS Code set —
/// the gray identity lives in the chrome, not in the code colors.
pub const GRAY: ThemePalette = ThemePalette {
    background: Rgb(0xF3, 0xF4, 0xF6),
    foreground: Rgb(0x1F, 0x29, 0x37),
    border: Rgb(0x9C, 0xA3, 0xAF),
    muted: Rgb(0x5F, 0x67, 0x74),
    accent: Rgb(0x4B, 0x55, 0x63),
    blue: Rgb(0x1E, 0x5F, 0xAB),
    sky_blue: Rgb(0x0E, 0x6B, 0x94),
    success: Rgb(0x0B, 0x7A, 0x35),
    warning: Rgb(0x8F, 0x5A, 0x00),
    error: Rgb(0xC5, 0x30, 0x30),
    purple: Rgb(0x77, 0x30, 0xA8),
    pink: Rgb(0xAD, 0x2E, 0x62),
    orange: Rgb(0xB9, 0x47, 0x00),
    response: ResponsePalette {
        heading: Rgb(0x37, 0x41, 0x51),
        link: Rgb(0x1E, 0x5F, 0xAB),
        inline_code: Rgb(0x77, 0x30, 0xA8),
    },
    model_gpt56: Rgb(0x1E, 0x5F, 0xAB),
    model_gpt55: Rgb(0x4B, 0x55, 0x63),
    model_opencode: Rgb(0x1E, 0x5F, 0xAB),
    model_astra: Rgb(0xC2, 0x41, 0x3E),
    model_sol: Rgb(0xB9, 0x47, 0x00),
    model_terra: Rgb(0x0B, 0x7A, 0x35),
    model_luna: Rgb(0x77, 0x30, 0xA8),
    model_spark: Rgb(0x8F, 0x5A, 0x00),
    agent_builder: Rgb(0x0E, 0x6B, 0x94),
    agent_planner: Rgb(0x0B, 0x7A, 0x35),
    agent_goal_runner: Rgb(0xC5, 0x30, 0x30),
    agent_reviewer: Rgb(0x77, 0x30, 0xA8),
    status: StatusLinePalette {
        text: Rgb(0x1F, 0x29, 0x37),
        separator: Rgb(0x5F, 0x67, 0x74),
        branch: Rgb(0x00, 0x91, 0xD1),
        context: Rgb(0x15, 0x80, 0x3D),
        model_haiku: Rgb(0x32, 0x6A, 0xA5),
        model_sonnet: Rgb(0xA1, 0x62, 0x07),
        model_opus: Rgb(0xC2, 0x41, 0x3E),
        model_fable: Rgb(0x76, 0x55, 0x8F),
        effort_low: Rgb(0xA1, 0x62, 0x07),
        effort_medium: Rgb(0x15, 0x80, 0x3D),
        effort_high: Rgb(0x76, 0x55, 0x8F),
        effort_xhigh: Rgb(0x65, 0x49, 0x7B),
        effort_max: Rgb(0xC2, 0x41, 0x3E),
        effort_ultra: Rgb(0xC2, 0x00, 0x78),
        five_hour: Rgb(0x32, 0x6A, 0xA5),
        weekly: Rgb(0x76, 0x55, 0x8F),
    },
    code: Rgb(0x1F, 0x29, 0x37),
    syntax_comment: Rgb(0x00, 0x80, 0x00),
    syntax_string: Rgb(0xA3, 0x15, 0x15),
    syntax_keyword: Rgb(0x1D, 0x4E, 0xD8),
    syntax_number: Rgb(0x05, 0x7A, 0x55),
    syntax_type: Rgb(0x0F, 0x76, 0x70),
    syntax_function: Rgb(0x79, 0x5E, 0x26),
    syntax_attribute: Rgb(0xB4, 0x53, 0x09),
    syntax_property: Rgb(0x1E, 0x5F, 0xAB),
    diff_add_bg: Rgb(0xE7, 0xF6, 0xEB),
    diff_remove_bg: Rgb(0xFC, 0xE8, 0xE8),
    diff_add_word_bg: Rgb(0x9E, 0xDD, 0xAE),
    diff_remove_word_bg: Rgb(0xF0, 0xAA, 0xB2),
    user_prompt_bg: Rgb(0xE2, 0xE5, 0xE9),
    model_change_bg: Rgb(0xE5, 0xE7, 0xEB),
    hover_bg: Rgb(0xE9, 0xEB, 0xEF),
    selection_bg: Rgb(0xD9, 0xDD, 0xE3),
    selection_fg: Rgb(0x1F, 0x29, 0x37),
};

/// DevezCode's `softpink` theme (`App.xaml.cs` + `devez-softpink.json`): blush
/// page, rose accent. Warm-leaning syntax so code does not read cold against it.
pub const SOFT_PINK: ThemePalette = ThemePalette {
    background: Rgb(0xFF, 0xF7, 0xFA),
    foreground: Rgb(0x3B, 0x29, 0x31),
    border: Rgb(0xD2, 0xA8, 0xBA),
    muted: Rgb(0x73, 0x57, 0x63),
    accent: Rgb(0xB5, 0x4A, 0x6B),
    blue: Rgb(0x32, 0x6A, 0x9F),
    sky_blue: Rgb(0x2F, 0x6B, 0x8C),
    success: Rgb(0x25, 0x72, 0x3C),
    warning: Rgb(0x9A, 0x65, 0x0B),
    error: Rgb(0xC2, 0x41, 0x3E),
    purple: Rgb(0x84, 0x58, 0x8F),
    pink: Rgb(0xB5, 0x4A, 0x6B),
    orange: Rgb(0xA1, 0x62, 0x07),
    response: ResponsePalette {
        heading: Rgb(0xB5, 0x4A, 0x6B),
        link: Rgb(0x32, 0x6A, 0x9F),
        inline_code: Rgb(0x84, 0x58, 0x8F),
    },
    model_gpt56: Rgb(0x32, 0x6A, 0x9F),
    model_gpt55: Rgb(0x2F, 0x6B, 0x8C),
    model_opencode: Rgb(0x32, 0x6A, 0x9F),
    model_astra: Rgb(0xC2, 0x41, 0x3E),
    model_sol: Rgb(0xC2, 0x41, 0x0C),
    model_terra: Rgb(0x25, 0x72, 0x3C),
    model_luna: Rgb(0x84, 0x58, 0x8F),
    model_spark: Rgb(0x9A, 0x65, 0x0B),
    agent_builder: Rgb(0x2F, 0x6B, 0x8C),
    agent_planner: Rgb(0x25, 0x72, 0x3C),
    agent_goal_runner: Rgb(0xC2, 0x41, 0x3E),
    agent_reviewer: Rgb(0x84, 0x58, 0x8F),
    status: StatusLinePalette {
        text: Rgb(0x3B, 0x29, 0x31),
        separator: Rgb(0x73, 0x57, 0x63),
        branch: Rgb(0x00, 0x8D, 0xCC),
        context: Rgb(0x25, 0x72, 0x3C),
        model_haiku: Rgb(0x32, 0x6A, 0x9F),
        model_sonnet: Rgb(0x9A, 0x65, 0x0B),
        model_opus: Rgb(0xC2, 0x41, 0x3E),
        model_fable: Rgb(0x84, 0x58, 0x8F),
        effort_low: Rgb(0x9A, 0x65, 0x0B),
        effort_medium: Rgb(0x25, 0x72, 0x3C),
        effort_high: Rgb(0x84, 0x58, 0x8F),
        effort_xhigh: Rgb(0x70, 0x46, 0x7E),
        effort_max: Rgb(0xC2, 0x41, 0x3E),
        effort_ultra: Rgb(0xBD, 0x00, 0x74),
        five_hour: Rgb(0x32, 0x6A, 0x9F),
        weekly: Rgb(0x84, 0x58, 0x8F),
    },
    code: Rgb(0x3B, 0x29, 0x31),
    syntax_comment: Rgb(0x3F, 0x76, 0x48),
    syntax_string: Rgb(0xA3, 0x2B, 0x3F),
    syntax_keyword: Rgb(0x33, 0x55, 0xCC),
    syntax_number: Rgb(0x2E, 0x6A, 0x4D),
    syntax_type: Rgb(0x16, 0x75, 0x8A),
    syntax_function: Rgb(0x84, 0x58, 0x8F),
    syntax_attribute: Rgb(0xA1, 0x62, 0x07),
    syntax_property: Rgb(0x32, 0x6A, 0x9F),
    diff_add_bg: Rgb(0xE9, 0xF5, 0xEC),
    diff_remove_bg: Rgb(0xFD, 0xE7, 0xE7),
    diff_add_word_bg: Rgb(0x9E, 0xD8, 0xAE),
    diff_remove_word_bg: Rgb(0xF2, 0xAF, 0xB6),
    user_prompt_bg: Rgb(0xF8, 0xDC, 0xE6),
    model_change_bg: Rgb(0xFC, 0xEF, 0xF4),
    hover_bg: Rgb(0xFA, 0xE8, 0xEF),
    selection_bg: Rgb(0xF2, 0xC9, 0xD7),
    selection_fg: Rgb(0x3B, 0x29, 0x31),
};

/// DevezCode's `midnight` theme (`App.xaml.cs` + `devez-midnight.json`): navy
/// page, azure accent. Dark's VS Code syntax set carries over unchanged.
pub const MIDNIGHT: ThemePalette = ThemePalette {
    background: Rgb(0x11, 0x18, 0x27),
    foreground: Rgb(0xE5, 0xE7, 0xEB),
    border: Rgb(0x4B, 0x5D, 0x75),
    muted: Rgb(0x9C, 0xA3, 0xAF),
    accent: Rgb(0x60, 0xA5, 0xFA),
    blue: Rgb(0x60, 0xA5, 0xFA),
    sky_blue: Rgb(0x93, 0xC5, 0xFD),
    success: Rgb(0x34, 0xD3, 0x99),
    warning: Rgb(0xFB, 0xBF, 0x24),
    error: Rgb(0xF8, 0x71, 0x71),
    purple: Rgb(0xA7, 0x8B, 0xFA),
    pink: Rgb(0xF4, 0x72, 0xB6),
    orange: Rgb(0xFB, 0x92, 0x3C),
    response: ResponsePalette {
        heading: Rgb(0x93, 0xC5, 0xFD),
        link: Rgb(0x60, 0xA5, 0xFA),
        inline_code: Rgb(0xA7, 0x8B, 0xFA),
    },
    model_gpt56: Rgb(0x00, 0xF0, 0xFF),
    model_gpt55: Rgb(0x60, 0xA5, 0xFA),
    model_opencode: Rgb(0x60, 0xA5, 0xFA),
    model_astra: Rgb(0xD7, 0x65, 0x64),
    model_sol: Rgb(0xFF, 0x9E, 0x59),
    model_terra: Rgb(0x34, 0xD3, 0x99),
    model_luna: Rgb(0xC4, 0xB5, 0xFD),
    model_spark: Rgb(0xFD, 0xE0, 0x47),
    agent_builder: Rgb(0x38, 0xBD, 0xF8),
    agent_planner: Rgb(0x5F, 0xBF, 0x7A),
    agent_goal_runner: Rgb(0xF8, 0x71, 0x71),
    agent_reviewer: Rgb(0xA7, 0x8B, 0xFA),
    status: StatusLinePalette {
        text: Rgb(0xCB, 0xD5, 0xE1),
        separator: Rgb(0x82, 0x90, 0xA0),
        branch: Rgb(0x82, 0xAC, 0xDA),
        context: Rgb(0x32, 0xB7, 0x86),
        model_haiku: Rgb(0x07, 0xDC, 0xDB),
        model_sonnet: Rgb(0xD9, 0xB1, 0x17),
        model_opus: Rgb(0xD7, 0x65, 0x64),
        model_fable: Rgb(0xCA, 0x6C, 0xD6),
        effort_low: Rgb(0xC0, 0x97, 0x14),
        effort_medium: Rgb(0x3C, 0x8A, 0x58),
        effort_high: Rgb(0x9B, 0xA1, 0xD6),
        effort_xhigh: Rgb(0x9A, 0x77, 0xDB),
        effort_max: Rgb(0xD7, 0x65, 0x64),
        effort_ultra: Rgb(0xDD, 0x7F, 0xB8),
        five_hour: Rgb(0x57, 0x91, 0xD7),
        weekly: Rgb(0x93, 0x7B, 0xD7),
    },
    code: Rgb(0xE5, 0xE7, 0xEB),
    syntax_comment: Rgb(0x6A, 0x99, 0x55),
    syntax_string: Rgb(0xCE, 0x91, 0x78),
    syntax_keyword: Rgb(0x56, 0x9C, 0xD6),
    syntax_number: Rgb(0xB5, 0xCE, 0xA8),
    syntax_type: Rgb(0x4E, 0xC9, 0xB0),
    syntax_function: Rgb(0xDC, 0xDC, 0xAA),
    syntax_attribute: Rgb(0xC5, 0x86, 0xC0),
    syntax_property: Rgb(0x9C, 0xDC, 0xFE),
    diff_add_bg: Rgb(0x16, 0x36, 0x2F),
    diff_remove_bg: Rgb(0x3B, 0x1F, 0x2B),
    diff_add_word_bg: Rgb(0x1E, 0x5E, 0x4C),
    diff_remove_word_bg: Rgb(0x7E, 0x33, 0x45),
    user_prompt_bg: Rgb(0x1E, 0x3A, 0x5F),
    model_change_bg: Rgb(0x1F, 0x29, 0x37),
    hover_bg: Rgb(0x1B, 0x24, 0x34),
    selection_bg: Rgb(0x2D, 0x4A, 0x6B),
    selection_fg: Rgb(0xE5, 0xE7, 0xEB),
};

#[cfg(not(test))]
static CURRENT_THEME: AtomicU8 = AtomicU8::new(2);

// The theme is process-wide in the app — one terminal, one palette. Under
// `cfg(test)` it is per-thread instead: tests run in parallel, and anything that
// builds a `Renderer` sets the theme, so a shared cell means one test's theme
// decides what another test paints.
#[cfg(test)]
thread_local! {
    static CURRENT_THEME: std::cell::Cell<u8> = const { std::cell::Cell::new(2) };
}

fn decode_theme(value: u8) -> ThemeKind {
    match value {
        0 => ThemeKind::Minimal,
        1 => ThemeKind::Soft,
        3 => ThemeKind::Gray,
        4 => ThemeKind::SoftPink,
        5 => ThemeKind::Midnight,
        _ => ThemeKind::Dark,
    }
}

#[cfg(not(test))]
pub fn current() -> ThemeKind {
    decode_theme(CURRENT_THEME.load(Ordering::Relaxed))
}

#[cfg(not(test))]
pub fn set_current(theme: ThemeKind) {
    CURRENT_THEME.store(theme.index() as u8, Ordering::Relaxed);
}

#[cfg(test)]
pub fn current() -> ThemeKind {
    CURRENT_THEME.with(|theme| decode_theme(theme.get()))
}

#[cfg(test)]
pub fn set_current(theme: ThemeKind) {
    CURRENT_THEME.with(|current| current.set(theme.index() as u8));
}

pub fn palette() -> &'static ThemePalette {
    palette_of(current())
}

pub fn palette_of(theme: ThemeKind) -> &'static ThemePalette {
    match theme {
        ThemeKind::Minimal => &MINIMAL,
        ThemeKind::Soft => &SOFT,
        ThemeKind::Dark => &DARK,
        ThemeKind::Gray => &GRAY,
        ThemeKind::SoftPink => &SOFT_PINK,
        ThemeKind::Midnight => &MIDNIGHT,
    }
}

pub fn load(cli_override: Option<&str>) -> Result<ThemeKind> {
    if let Some(value) = cli_override {
        return ThemeKind::parse(value)
            .with_context(|| format!("지원하지 않는 테마입니다: {value}"));
    }
    Ok(read_theme_file(&devez_vibe_theme_file())
        .or_else(|| read_theme_file(&devez_code_theme_file()))
        .unwrap_or(ThemeKind::Dark))
}

pub fn save(theme: ThemeKind) -> Result<()> {
    let path = devez_vibe_theme_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("테마 설정 폴더 생성 실패: {}", parent.display()))?;
    }
    fs::write(&path, theme.id()).with_context(|| format!("테마 설정 저장 실패: {}", path.display()))
}

fn read_theme_file(path: &Path) -> Option<ThemeKind> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| ThemeKind::parse(&value))
}

fn app_data() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn devez_vibe_theme_file() -> PathBuf {
    app_data().join("DevezVibe").join("theme.txt")
}

fn devez_code_theme_file() -> PathBuf {
    app_data().join("DevezCode").join("theme.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_parse_to_the_devez_code_themes() {
        assert_eq!(ThemeKind::parse("minimal"), Some(ThemeKind::Minimal));
        assert_eq!(ThemeKind::parse("warm"), Some(ThemeKind::Soft));
        assert_eq!(ThemeKind::parse("dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("grey"), Some(ThemeKind::Gray));
        assert_eq!(ThemeKind::parse("softpink"), Some(ThemeKind::SoftPink));
        assert_eq!(ThemeKind::parse("midnight"), Some(ThemeKind::Midnight));
        assert_eq!(ThemeKind::parse("unknown"), None);
    }

    /// The on-disk value is the `ALL` index, so a saved theme must decode back
    /// to itself — appending themes must not shuffle the existing three.
    #[test]
    fn every_theme_round_trips_through_its_stored_index() {
        for theme in ThemeKind::ALL {
            assert_eq!(decode_theme(theme.index() as u8), theme);
            assert_eq!(ThemeKind::parse(theme.id()), Some(theme));
        }
    }

    #[test]
    fn every_theme_has_readable_core_contrast() {
        for theme in ThemeKind::ALL {
            let palette = palette_of(theme);
            let text_colors = [
                ("foreground", palette.foreground),
                ("muted", palette.muted),
                ("accent", palette.accent),
                ("blue", palette.blue),
                ("sky_blue", palette.sky_blue),
                ("model_opencode", palette.model_opencode),
                ("model_astra", palette.model_astra),
                ("success", palette.success),
                ("warning", palette.warning),
                ("error", palette.error),
                ("purple", palette.purple),
                ("pink", palette.pink),
                ("orange", palette.orange),
                ("response.heading", palette.response.heading),
                ("response.link", palette.response.link),
                ("response.inline_code", palette.response.inline_code),
                ("code", palette.code),
                ("syntax_comment", palette.syntax_comment),
                ("syntax_string", palette.syntax_string),
                ("syntax_keyword", palette.syntax_keyword),
                ("syntax_number", palette.syntax_number),
                ("syntax_type", palette.syntax_type),
                ("syntax_function", palette.syntax_function),
                ("syntax_attribute", palette.syntax_attribute),
                ("syntax_property", palette.syntax_property),
                ("agent_builder", palette.agent_builder),
                ("agent_planner", palette.agent_planner),
                ("agent_goal_runner", palette.agent_goal_runner),
                ("agent_reviewer", palette.agent_reviewer),
            ];
            for (name, color) in text_colors {
                let ratio = contrast_ratio(color, palette.background);
                assert!(
                    ratio >= 4.5,
                    "{} {name} contrast is only {ratio:.2}:1",
                    theme.display_name()
                );
            }
            assert!(
                contrast_ratio(palette.foreground, palette.diff_add_bg) >= 4.5,
                "{} added diff text is not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.diff_remove_bg) >= 4.5,
                "{} removed diff text is not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.diff_add_word_bg) >= 4.5,
                "{} added diff words are not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.diff_remove_word_bg) >= 4.5,
                "{} removed diff words are not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.user_prompt_bg) >= 4.5,
                "{} prompt card text is not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.model_change_bg) >= 4.5,
                "{} change card text is not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.foreground, palette.hover_bg) >= 4.5,
                "{} hover text is not readable",
                theme.display_name()
            );
            assert_ne!(palette.syntax_keyword, palette.syntax_type);
            assert_ne!(palette.syntax_keyword, palette.syntax_string);
        }
    }

    #[test]
    fn astra_matches_claude_opus_in_every_theme() {
        for theme in ThemeKind::ALL {
            let palette = palette_of(theme);
            assert_eq!(palette.model_astra, palette.status.model_opus);
        }
    }

    /// Neighbouring status line segments must not share a color, or `branch`,
    /// `ctx:`, `5h:` and `week:` blur into one stripe. The effort ramp is
    /// checked on its own because only one of its levels shows at a time.
    #[test]
    fn status_line_segments_never_share_a_color() {
        for theme in ThemeKind::ALL {
            let palette = palette_of(theme);
            let status = palette.status;
            assert_all_distinct(
                theme,
                &[
                    ("branch", status.branch),
                    ("ctx", status.context),
                    ("5h", status.five_hour),
                    ("week", status.weekly),
                    ("separator", status.separator),
                ],
            );
            assert_all_distinct(
                theme,
                &[
                    ("eff: low", status.effort_low),
                    ("eff: medium", status.effort_medium),
                    ("eff: high", status.effort_high),
                    ("eff: xhigh", status.effort_xhigh),
                    ("eff: max", status.effort_max),
                    ("eff: ultra", status.effort_ultra),
                ],
            );
        }
    }

    /// The status line is exempt from the 4.5:1 bar that
    /// `every_theme_has_readable_core_contrast` holds the rest of the palette
    /// to, because its colors mirror DevezCode's shared statusline script,
    /// which favours hue separation over contrast on light backgrounds. This
    /// floor is only a backstop against a segment going invisible: the lowest
    /// today is Soft `eff: low` at 2.17:1.
    #[test]
    fn status_line_colors_stay_visible() {
        for theme in ThemeKind::ALL {
            let palette = palette_of(theme);
            let status = palette.status;
            let segments = [
                ("text", status.text),
                ("separator", status.separator),
                ("branch", status.branch),
                ("ctx", status.context),
                ("eff: low", status.effort_low),
                ("eff: medium", status.effort_medium),
                ("eff: high", status.effort_high),
                ("eff: xhigh", status.effort_xhigh),
                ("eff: max", status.effort_max),
                ("eff: ultra", status.effort_ultra),
                ("5h", status.five_hour),
                ("week", status.weekly),
            ];
            for (name, color) in segments {
                let ratio = contrast_ratio(color, palette.background);
                assert!(
                    ratio >= 2.0,
                    "{} status {name} contrast is only {ratio:.2}:1",
                    theme.display_name()
                );
            }
        }
    }

    #[test]
    fn claude_status_colors_match_the_devez_code_script() {
        let expected = [
            (
                ThemeKind::Minimal,
                [
                    Rgb(0x00, 0xD1, 0xD1),
                    Rgb(0xCA, 0x8A, 0x04),
                    Rgb(0xD1, 0x00, 0x00),
                    Rgb(0xB5, 0x00, 0xD1),
                ],
                [
                    Rgb(0xD1, 0x9F, 0x00),
                    Rgb(0x00, 0xA7, 0x40),
                    Rgb(0x43, 0x38, 0xCA),
                    Rgb(0x7C, 0x00, 0xD1),
                    Rgb(0xD1, 0x00, 0x00),
                ],
            ),
            (
                ThemeKind::Soft,
                [
                    Rgb(0x00, 0xCC, 0xCC),
                    Rgb(0xC9, 0x7C, 0x1A),
                    Rgb(0xCC, 0x00, 0x00),
                    Rgb(0xB1, 0x00, 0xCC),
                ],
                [
                    Rgb(0xCC, 0x9C, 0x00),
                    Rgb(0x00, 0xA3, 0x3F),
                    Rgb(0x43, 0x38, 0xCA),
                    Rgb(0x79, 0x00, 0xCC),
                    Rgb(0xCC, 0x00, 0x00),
                ],
            ),
            (
                ThemeKind::Dark,
                [
                    Rgb(0x07, 0xDC, 0xDB),
                    Rgb(0xD9, 0xB1, 0x17),
                    Rgb(0xD7, 0x65, 0x64),
                    Rgb(0xCA, 0x6C, 0xD6),
                ],
                [
                    Rgb(0xC0, 0x97, 0x14),
                    Rgb(0x3C, 0x8A, 0x58),
                    Rgb(0x9B, 0xA1, 0xD6),
                    Rgb(0x9A, 0x77, 0xDB),
                    Rgb(0xD7, 0x65, 0x64),
                ],
            ),
            (
                ThemeKind::Gray,
                [
                    Rgb(0x32, 0x6A, 0xA5),
                    Rgb(0xA1, 0x62, 0x07),
                    Rgb(0xC2, 0x41, 0x3E),
                    Rgb(0x76, 0x55, 0x8F),
                ],
                [
                    Rgb(0xA1, 0x62, 0x07),
                    Rgb(0x15, 0x80, 0x3D),
                    Rgb(0x76, 0x55, 0x8F),
                    Rgb(0x65, 0x49, 0x7B),
                    Rgb(0xC2, 0x41, 0x3E),
                ],
            ),
            (
                ThemeKind::SoftPink,
                [
                    Rgb(0x32, 0x6A, 0x9F),
                    Rgb(0x9A, 0x65, 0x0B),
                    Rgb(0xC2, 0x41, 0x3E),
                    Rgb(0x84, 0x58, 0x8F),
                ],
                [
                    Rgb(0x9A, 0x65, 0x0B),
                    Rgb(0x25, 0x72, 0x3C),
                    Rgb(0x84, 0x58, 0x8F),
                    Rgb(0x70, 0x46, 0x7E),
                    Rgb(0xC2, 0x41, 0x3E),
                ],
            ),
            (
                ThemeKind::Midnight,
                [
                    Rgb(0x07, 0xDC, 0xDB),
                    Rgb(0xD9, 0xB1, 0x17),
                    Rgb(0xD7, 0x65, 0x64),
                    Rgb(0xCA, 0x6C, 0xD6),
                ],
                [
                    Rgb(0xC0, 0x97, 0x14),
                    Rgb(0x3C, 0x8A, 0x58),
                    Rgb(0x9B, 0xA1, 0xD6),
                    Rgb(0x9A, 0x77, 0xDB),
                    Rgb(0xD7, 0x65, 0x64),
                ],
            ),
        ];

        for (theme, models, efforts) in expected {
            let status = palette_of(theme).status;
            assert_eq!(
                [
                    status.model_haiku,
                    status.model_sonnet,
                    status.model_opus,
                    status.model_fable
                ],
                models
            );
            assert_eq!(
                [
                    status.effort_low,
                    status.effort_medium,
                    status.effort_high,
                    status.effort_xhigh,
                    status.effort_max
                ],
                efforts
            );
        }
    }

    #[test]
    fn claude_status_usage_colors_match_the_devez_code_script() {
        let expected = [
            (
                ThemeKind::Minimal,
                [
                    Rgb(0x00, 0x8E, 0x5A),
                    Rgb(0x00, 0x5E, 0xD1),
                    Rgb(0x35, 0x00, 0xD1),
                ],
            ),
            (
                ThemeKind::Soft,
                [
                    Rgb(0x00, 0x8B, 0x58),
                    Rgb(0x00, 0x5B, 0xCC),
                    Rgb(0x33, 0x00, 0xCC),
                ],
            ),
            (
                ThemeKind::Dark,
                [
                    Rgb(0x32, 0xB7, 0x86),
                    Rgb(0x57, 0x91, 0xD7),
                    Rgb(0x93, 0x7B, 0xD7),
                ],
            ),
            (
                ThemeKind::Gray,
                [
                    Rgb(0x15, 0x80, 0x3D),
                    Rgb(0x32, 0x6A, 0xA5),
                    Rgb(0x76, 0x55, 0x8F),
                ],
            ),
            (
                ThemeKind::SoftPink,
                [
                    Rgb(0x25, 0x72, 0x3C),
                    Rgb(0x32, 0x6A, 0x9F),
                    Rgb(0x84, 0x58, 0x8F),
                ],
            ),
            (
                ThemeKind::Midnight,
                [
                    Rgb(0x32, 0xB7, 0x86),
                    Rgb(0x57, 0x91, 0xD7),
                    Rgb(0x93, 0x7B, 0xD7),
                ],
            ),
        ];

        for (theme, usage) in expected {
            let status = palette_of(theme).status;
            assert_eq!(
                [status.context, status.five_hour, status.weekly],
                usage,
                "{} usage colors differ from DevezCode",
                theme.display_name()
            );
        }
    }

    /// A role is read off its colour, so two roles sharing one would make the
    /// composer rule ambiguous about which role the next turn is sent under.
    #[test]
    fn agent_roles_never_share_a_color() {
        for theme in ThemeKind::ALL {
            let palette = palette_of(theme);
            assert_all_distinct(
                theme,
                &[
                    ("builder", palette.agent_builder),
                    ("planner", palette.agent_planner),
                    ("goal runner", palette.agent_goal_runner),
                    ("reviewer", palette.agent_reviewer),
                ],
            );
        }
    }

    fn assert_all_distinct(theme: ThemeKind, segments: &[(&str, Rgb)]) {
        for (index, (name, color)) in segments.iter().enumerate() {
            for (other_name, other_color) in &segments[index + 1..] {
                assert_ne!(
                    color,
                    other_color,
                    "{} paints {name} and {other_name} the same",
                    theme.display_name()
                );
            }
        }
    }
}
