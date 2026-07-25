use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
};

use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    Minimal,
    Soft,
    Dark,
}

impl ThemeKind {
    pub const ALL: [Self; 3] = [Self::Minimal, Self::Soft, Self::Dark];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" | "min" | "light" => Some(Self::Minimal),
            "soft" | "warm" => Some(Self::Soft),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Soft => "soft",
            Self::Dark => "dark",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Minimal => "Minimal",
            Self::Soft => "Soft",
            Self::Dark => "Dark",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Minimal => "cool white · blue accent",
            Self::Soft => "warm cream · green accent",
            Self::Dark => "charcoal · orange accent",
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

#[derive(Clone, Copy)]
pub struct ThemePalette {
    pub background: Rgb,
    pub foreground: Rgb,
    pub border: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub blue: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
    pub error: Rgb,
    pub purple: Rgb,
    pub pink: Rgb,
    pub orange: Rgb,
    pub status: StatusLinePalette,
    pub code: Rgb,
    pub syntax_comment: Rgb,
    pub syntax_string: Rgb,
    pub syntax_keyword: Rgb,
    pub syntax_number: Rgb,
    pub syntax_type: Rgb,
    pub syntax_function: Rgb,
    pub diff_add: Rgb,
    pub diff_remove: Rgb,
    pub diff_header: Rgb,
    pub diff_add_bg: Rgb,
    pub diff_remove_bg: Rgb,
    pub user_prompt_bg: Rgb,
    pub model_change_bg: Rgb,
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
    pub context: Rgb,
    pub effort_low: Rgb,
    pub effort_medium: Rgb,
    pub effort_high: Rgb,
    pub effort_xhigh: Rgb,
    pub effort_max: Rgb,
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
    success: Rgb(0x15, 0x80, 0x3D),
    warning: Rgb(0x9A, 0x67, 0x00),
    error: Rgb(0xDC, 0x26, 0x26),
    purple: Rgb(0x7C, 0x3A, 0xED),
    pink: Rgb(0xBE, 0x18, 0x5D),
    orange: Rgb(0xBC, 0x4C, 0x00),
    status: StatusLinePalette {
        text: Rgb(0x0F, 0x14, 0x22),
        separator: Rgb(0x0F, 0x14, 0x22),
        branch: Rgb(0x00, 0x91, 0xD1),
        context: Rgb(0x00, 0x8E, 0x5A),
        effort_low: Rgb(0xD1, 0x9F, 0x00),
        effort_medium: Rgb(0x00, 0xA7, 0x40),
        effort_high: Rgb(0x43, 0x38, 0xCA),
        effort_xhigh: Rgb(0x7C, 0x00, 0xD1),
        effort_max: Rgb(0xD1, 0x00, 0x00),
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
    diff_add: Rgb(0x11, 0x63, 0x29),
    diff_remove: Rgb(0xA4, 0x0E, 0x26),
    diff_header: Rgb(0x05, 0x63, 0xC1),
    diff_add_bg: Rgb(0xDA, 0xF0, 0xDE),
    diff_remove_bg: Rgb(0xF1, 0xD7, 0xDA),
    user_prompt_bg: Rgb(0xEE, 0xF4, 0xFF),
    model_change_bg: Rgb(0xDB, 0xEA, 0xFE),
};

pub const SOFT: ThemePalette = ThemePalette {
    background: Rgb(0xF2, 0xED, 0xE6),
    foreground: Rgb(0x2A, 0x26, 0x20),
    border: Rgb(0x9A, 0x91, 0x7F),
    muted: Rgb(0x5A, 0x54, 0x48),
    accent: Rgb(0x42, 0x68, 0x34),
    blue: Rgb(0x42, 0x63, 0x8F),
    success: Rgb(0x16, 0x65, 0x34),
    warning: Rgb(0x8A, 0x4B, 0x08),
    error: Rgb(0xA3, 0x3E, 0x3E),
    purple: Rgb(0x68, 0x4B, 0x8A),
    pink: Rgb(0x8F, 0x3D, 0x5A),
    orange: Rgb(0x9A, 0x4D, 0x12),
    status: StatusLinePalette {
        text: Rgb(0x16, 0x12, 0x0C),
        separator: Rgb(0x16, 0x12, 0x0C),
        branch: Rgb(0x00, 0x8D, 0xCC),
        context: Rgb(0x00, 0x8B, 0x58),
        effort_low: Rgb(0xCC, 0x9C, 0x00),
        effort_medium: Rgb(0x00, 0xA3, 0x3F),
        effort_high: Rgb(0x43, 0x38, 0xCA),
        effort_xhigh: Rgb(0x79, 0x00, 0xCC),
        effort_max: Rgb(0xCC, 0x00, 0x00),
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
    diff_add: Rgb(0x1F, 0x6D, 0x32),
    diff_remove: Rgb(0x96, 0x34, 0x3A),
    diff_header: Rgb(0x3A, 0x6F, 0xA5),
    diff_add_bg: Rgb(0xD9, 0xE5, 0xD8),
    diff_remove_bg: Rgb(0xEC, 0xD1, 0xD2),
    user_prompt_bg: Rgb(0xE6, 0xF0, 0xDE),
    model_change_bg: Rgb(0xDE, 0xEC, 0xD6),
};

pub const DARK: ThemePalette = ThemePalette {
    background: Rgb(0x1F, 0x1F, 0x1E),
    foreground: Rgb(0xE8, 0xE8, 0xE8),
    border: Rgb(0x77, 0x77, 0x77),
    muted: Rgb(0xAA, 0xAA, 0xAA),
    accent: Rgb(0xD9, 0x77, 0x3F),
    blue: Rgb(0x60, 0xA5, 0xFA),
    success: Rgb(0x22, 0xC5, 0x5E),
    warning: Rgb(0xFB, 0xBF, 0x24),
    error: Rgb(0xF8, 0x71, 0x71),
    purple: Rgb(0xA7, 0x8B, 0xFA),
    pink: Rgb(0xF4, 0x72, 0xB6),
    orange: Rgb(0xFB, 0x92, 0x3C),
    status: StatusLinePalette {
        text: Rgb(0xC7, 0xC8, 0xCB),
        separator: Rgb(0x82, 0x90, 0xA0),
        branch: Rgb(0x82, 0xAC, 0xDA),
        context: Rgb(0x32, 0xB7, 0x86),
        effort_low: Rgb(0xC0, 0x97, 0x14),
        effort_medium: Rgb(0x3C, 0x8A, 0x58),
        effort_high: Rgb(0x9B, 0xA1, 0xD6),
        effort_xhigh: Rgb(0x9A, 0x77, 0xDB),
        effort_max: Rgb(0xD7, 0x65, 0x64),
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
    diff_add: Rgb(0x56, 0xD3, 0x64),
    diff_remove: Rgb(0xFF, 0x7B, 0x72),
    diff_header: Rgb(0x4F, 0xA6, 0xFF),
    diff_add_bg: Rgb(0x26, 0x3D, 0x2A),
    diff_remove_bg: Rgb(0x47, 0x29, 0x28),
    user_prompt_bg: Rgb(0x36, 0x36, 0x36),
    model_change_bg: Rgb(0x43, 0x43, 0x43),
};

static CURRENT_THEME: AtomicU8 = AtomicU8::new(2);

pub fn current() -> ThemeKind {
    match CURRENT_THEME.load(Ordering::Relaxed) {
        0 => ThemeKind::Minimal,
        1 => ThemeKind::Soft,
        _ => ThemeKind::Dark,
    }
}

pub fn set_current(theme: ThemeKind) {
    CURRENT_THEME.store(theme.index() as u8, Ordering::Relaxed);
}

pub fn palette() -> &'static ThemePalette {
    match current() {
        ThemeKind::Minimal => &MINIMAL,
        ThemeKind::Soft => &SOFT,
        ThemeKind::Dark => &DARK,
    }
}

pub fn load(cli_override: Option<&str>) -> Result<ThemeKind> {
    if let Some(value) = cli_override {
        return ThemeKind::parse(value)
            .with_context(|| format!("지원하지 않는 테마입니다: {value}"));
    }
    Ok(read_theme_file(&devez_cli_theme_file())
        .or_else(|| read_theme_file(&devez_code_theme_file()))
        .unwrap_or(ThemeKind::Dark))
}

pub fn save(theme: ThemeKind) -> Result<()> {
    let path = devez_cli_theme_file();
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

fn devez_cli_theme_file() -> PathBuf {
    app_data().join("DevezCLI").join("theme.txt")
}

fn devez_code_theme_file() -> PathBuf {
    app_data().join("DevezCode").join("theme.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_parse_to_the_three_devez_code_themes() {
        assert_eq!(ThemeKind::parse("minimal"), Some(ThemeKind::Minimal));
        assert_eq!(ThemeKind::parse("warm"), Some(ThemeKind::Soft));
        assert_eq!(ThemeKind::parse("dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("unknown"), None);
    }

    #[test]
    fn every_theme_has_readable_core_contrast() {
        for theme in ThemeKind::ALL {
            let palette = match theme {
                ThemeKind::Minimal => MINIMAL,
                ThemeKind::Soft => SOFT,
                ThemeKind::Dark => DARK,
            };
            let text_colors = [
                ("foreground", palette.foreground),
                ("muted", palette.muted),
                ("accent", palette.accent),
                ("blue", palette.blue),
                ("success", palette.success),
                ("warning", palette.warning),
                ("error", palette.error),
                ("purple", palette.purple),
                ("pink", palette.pink),
                ("orange", palette.orange),
                ("code", palette.code),
                ("syntax_comment", palette.syntax_comment),
                ("syntax_string", palette.syntax_string),
                ("syntax_keyword", palette.syntax_keyword),
                ("syntax_number", palette.syntax_number),
                ("syntax_type", palette.syntax_type),
                ("syntax_function", palette.syntax_function),
                ("diff_header", palette.diff_header),
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
                contrast_ratio(palette.diff_add, palette.diff_add_bg) >= 4.5,
                "{} added diff text is not readable",
                theme.display_name()
            );
            assert!(
                contrast_ratio(palette.diff_remove, palette.diff_remove_bg) >= 4.5,
                "{} removed diff text is not readable",
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
            assert_ne!(palette.syntax_keyword, palette.syntax_type);
            assert_ne!(palette.syntax_keyword, palette.syntax_string);
        }
    }

    /// Neighbouring status line segments must not share a color, or `branch`,
    /// `ctx:`, `5h:` and `week:` blur into one stripe. The effort ramp is
    /// checked on its own because only one of its levels shows at a time.
    #[test]
    fn status_line_segments_never_share_a_color() {
        for theme in ThemeKind::ALL {
            let palette = match theme {
                ThemeKind::Minimal => MINIMAL,
                ThemeKind::Soft => SOFT,
                ThemeKind::Dark => DARK,
            };
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
            let palette = match theme {
                ThemeKind::Minimal => MINIMAL,
                ThemeKind::Soft => SOFT,
                ThemeKind::Dark => DARK,
            };
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

    fn contrast_ratio(left: Rgb, right: Rgb) -> f64 {
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
}
