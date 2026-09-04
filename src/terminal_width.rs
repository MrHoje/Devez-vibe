use std::borrow::Cow;

#[cfg(test)]
use std::cell::Cell as TestCell;
#[cfg(not(test))]
use std::{env, sync::OnceLock};

use textwrap::{
    core::Fragment,
    wrap_algorithms::{Penalties, wrap_first_fit, wrap_optimal_fit},
};
use unicode_width::{
    UnicodeWidthChar as ModernUnicodeWidthChar, UnicodeWidthStr as ModernUnicodeWidthStr,
};

/// Width API shared by every terminal-facing module. Standalone dvz keeps
/// modern Unicode widths; DevezCode's explicit profile follows the bundled
/// xterm 6 Unicode 6 provider instead.
pub(crate) struct UnicodeWidthChar;
pub(crate) struct UnicodeWidthStr;

impl UnicodeWidthChar {
    pub(crate) fn width(ch: char) -> Option<usize> {
        if !use_devezcode_xterm_widths() {
            return ModernUnicodeWidthChar::width(ch);
        }
        Some(xterm_unicode6_width(ch as u32))
    }
}

impl UnicodeWidthStr {
    pub(crate) fn width(text: &str) -> usize {
        if !use_devezcode_xterm_widths() {
            return ModernUnicodeWidthStr::width(text);
        }
        text.chars()
            .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum()
    }
}

/// Characters this build and the host may size differently. The xterm 6 profile
/// keeps a modern emoji at one cell, while the console layer that replays our
/// output sizes it as two. A mid-row patch on such a row therefore comes back
/// with its columns shifted, exactly as a wide glyph does, so callers treat
/// those rows as wide and reprint them whole from column zero.
pub(crate) fn width_may_differ_on_host(ch: char) -> bool {
    UnicodeWidthChar::width(ch).unwrap_or(0) != ModernUnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Exact combining ranges from the bundled xterm 6 `UnicodeV6.ts` provider.
/// Newer combining marks deliberately stay width 1 because that is what the
/// actual host buffer does; mixing Unicode versions is the bug this module prevents.
const XTERM6_BMP_COMBINING: &[(u32, u32)] = &[
    (0x0300, 0x036f),
    (0x0483, 0x0486),
    (0x0488, 0x0489),
    (0x0591, 0x05bd),
    (0x05bf, 0x05bf),
    (0x05c1, 0x05c2),
    (0x05c4, 0x05c5),
    (0x05c7, 0x05c7),
    (0x0600, 0x0603),
    (0x0610, 0x0615),
    (0x064b, 0x065e),
    (0x0670, 0x0670),
    (0x06d6, 0x06e4),
    (0x06e7, 0x06e8),
    (0x06ea, 0x06ed),
    (0x070f, 0x070f),
    (0x0711, 0x0711),
    (0x0730, 0x074a),
    (0x07a6, 0x07b0),
    (0x07eb, 0x07f3),
    (0x0901, 0x0902),
    (0x093c, 0x093c),
    (0x0941, 0x0948),
    (0x094d, 0x094d),
    (0x0951, 0x0954),
    (0x0962, 0x0963),
    (0x0981, 0x0981),
    (0x09bc, 0x09bc),
    (0x09c1, 0x09c4),
    (0x09cd, 0x09cd),
    (0x09e2, 0x09e3),
    (0x0a01, 0x0a02),
    (0x0a3c, 0x0a3c),
    (0x0a41, 0x0a42),
    (0x0a47, 0x0a48),
    (0x0a4b, 0x0a4d),
    (0x0a70, 0x0a71),
    (0x0a81, 0x0a82),
    (0x0abc, 0x0abc),
    (0x0ac1, 0x0ac5),
    (0x0ac7, 0x0ac8),
    (0x0acd, 0x0acd),
    (0x0ae2, 0x0ae3),
    (0x0b01, 0x0b01),
    (0x0b3c, 0x0b3c),
    (0x0b3f, 0x0b3f),
    (0x0b41, 0x0b43),
    (0x0b4d, 0x0b4d),
    (0x0b56, 0x0b56),
    (0x0b82, 0x0b82),
    (0x0bc0, 0x0bc0),
    (0x0bcd, 0x0bcd),
    (0x0c3e, 0x0c40),
    (0x0c46, 0x0c48),
    (0x0c4a, 0x0c4d),
    (0x0c55, 0x0c56),
    (0x0cbc, 0x0cbc),
    (0x0cbf, 0x0cbf),
    (0x0cc6, 0x0cc6),
    (0x0ccc, 0x0ccd),
    (0x0ce2, 0x0ce3),
    (0x0d41, 0x0d43),
    (0x0d4d, 0x0d4d),
    (0x0dca, 0x0dca),
    (0x0dd2, 0x0dd4),
    (0x0dd6, 0x0dd6),
    (0x0e31, 0x0e31),
    (0x0e34, 0x0e3a),
    (0x0e47, 0x0e4e),
    (0x0eb1, 0x0eb1),
    (0x0eb4, 0x0eb9),
    (0x0ebb, 0x0ebc),
    (0x0ec8, 0x0ecd),
    (0x0f18, 0x0f19),
    (0x0f35, 0x0f35),
    (0x0f37, 0x0f37),
    (0x0f39, 0x0f39),
    (0x0f71, 0x0f7e),
    (0x0f80, 0x0f84),
    (0x0f86, 0x0f87),
    (0x0f90, 0x0f97),
    (0x0f99, 0x0fbc),
    (0x0fc6, 0x0fc6),
    (0x102d, 0x1030),
    (0x1032, 0x1032),
    (0x1036, 0x1037),
    (0x1039, 0x1039),
    (0x1058, 0x1059),
    (0x1160, 0x11ff),
    (0x135f, 0x135f),
    (0x1712, 0x1714),
    (0x1732, 0x1734),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17b4, 0x17b5),
    (0x17b7, 0x17bd),
    (0x17c6, 0x17c6),
    (0x17c9, 0x17d3),
    (0x17dd, 0x17dd),
    (0x180b, 0x180d),
    (0x18a9, 0x18a9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193b),
    (0x1a17, 0x1a18),
    (0x1b00, 0x1b03),
    (0x1b34, 0x1b34),
    (0x1b36, 0x1b3a),
    (0x1b3c, 0x1b3c),
    (0x1b42, 0x1b42),
    (0x1b6b, 0x1b73),
    (0x1dc0, 0x1dca),
    (0x1dfe, 0x1dff),
    (0x200b, 0x200f),
    (0x202a, 0x202e),
    (0x2060, 0x2063),
    (0x206a, 0x206f),
    (0x20d0, 0x20ef),
    (0x302a, 0x302f),
    (0x3099, 0x309a),
    (0xa806, 0xa806),
    (0xa80b, 0xa80b),
    (0xa825, 0xa826),
    (0xfb1e, 0xfb1e),
    (0xfe00, 0xfe0f),
    (0xfe20, 0xfe23),
    (0xfeff, 0xfeff),
    (0xfff9, 0xfffb),
];


fn in_sorted_ranges(codepoint: u32, ranges: &[(u32, u32)]) -> bool {
    ranges
        .binary_search_by(|&(start, end)| {
            if codepoint < start {
                std::cmp::Ordering::Greater
            } else if codepoint > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Codepoints the Windows console lays out as two cells even though the modern
/// and xterm 6 tables both call them one. Measured directly on this platform by
/// writing each character and reading the cursor column back, because the
/// console -- not the web renderer -- owns the cell buffer our output is
/// replayed through. The middle dot and the arrows in this table sit in the
/// status line and hints of every frame, so a one-cell assumption pushes those
/// rows past the right edge and wraps the whole screen down a line.
const CONSOLE_WIDE_AMBIGUOUS: &[(u32, u32)] = &[
    (0x00a1, 0x00a1),
    (0x00a4, 0x00a4),
    (0x00a7, 0x00a8),
    (0x00aa, 0x00aa),
    (0x00ad, 0x00ae),
    (0x00b0, 0x00b4),
    (0x00b6, 0x00ba),
    (0x00bc, 0x00bf),
    (0x00c6, 0x00c6),
    (0x00d0, 0x00d0),
    (0x00d7, 0x00d8),
    (0x00de, 0x00df),
    (0x00e6, 0x00e6),
    (0x00f0, 0x00f0),
    (0x00f7, 0x00f8),
    (0x00fe, 0x00fe),
    (0x0111, 0x0111),
    (0x0126, 0x0127),
    (0x0131, 0x0133),
    (0x0138, 0x0138),
    (0x013f, 0x0142),
    (0x0149, 0x014b),
    (0x0152, 0x0153),
    (0x0166, 0x0167),
    (0x02c7, 0x02c7),
    (0x02d0, 0x02d0),
    (0x02d8, 0x02db),
    (0x02dd, 0x02dd),
    (0x0391, 0x03a1),
    (0x03a3, 0x03a9),
    (0x03b1, 0x03c1),
    (0x03c3, 0x03c9),
    (0x0401, 0x0401),
    (0x0410, 0x044f),
    (0x0451, 0x0451),
    (0x2015, 0x2015),
    (0x2018, 0x2019),
    (0x201c, 0x201d),
    (0x2020, 0x2021),
    (0x2025, 0x2026),
    (0x2030, 0x2030),
    (0x2032, 0x2033),
    (0x203b, 0x203b),
    (0x2074, 0x2074),
    (0x207f, 0x207f),
    (0x2081, 0x2084),
    (0x20ac, 0x20ac),
    (0x2103, 0x2103),
    (0x2109, 0x2109),
    (0x2113, 0x2113),
    (0x2116, 0x2116),
    (0x2121, 0x2122),
    (0x2126, 0x2126),
    (0x212b, 0x212b),
    (0x2153, 0x2154),
    (0x215b, 0x215e),
    (0x2160, 0x2169),
    (0x2170, 0x2179),
    (0x2190, 0x2199),
    (0x21d2, 0x21d2),
    (0x21d4, 0x21d4),
    (0x2200, 0x2200),
    (0x2202, 0x2203),
    (0x2207, 0x2208),
    (0x220b, 0x220b),
    (0x220f, 0x220f),
    (0x2211, 0x2211),
    (0x221a, 0x221a),
    (0x221d, 0x221e),
    (0x2220, 0x2220),
    (0x2225, 0x2225),
    (0x2227, 0x222c),
    (0x222e, 0x222e),
    (0x2234, 0x2235),
    (0x223c, 0x223d),
    (0x2252, 0x2252),
    (0x2260, 0x2261),
    (0x2264, 0x2265),
    (0x226a, 0x226b),
    (0x2282, 0x2283),
    (0x2286, 0x2287),
    (0x2299, 0x2299),
    (0x22a5, 0x22a5),
    (0x2312, 0x2312),
    (0x2460, 0x246e),
    (0x2474, 0x2482),
    (0x249c, 0x24e9),
    (0x25a0, 0x25a1),
    (0x25a3, 0x25a9),
    (0x25b2, 0x25b3),
    (0x25b6, 0x25b7),
    (0x25bc, 0x25bd),
    (0x25c0, 0x25c1),
    (0x25c6, 0x25c8),
    (0x25cb, 0x25cb),
    (0x25ce, 0x25d1),
    (0x2605, 0x2606),
    (0x260e, 0x260f),
    (0x261c, 0x261c),
    (0x261e, 0x261e),
    (0x2640, 0x2640),
    (0x2642, 0x2642),
    (0x2660, 0x2661),
    (0x2663, 0x2665),
    (0x2667, 0x266a),
    (0x266c, 0x266d),
    (0xe000, 0xe00c),
    (0xe00e, 0xe011),
    (0xe016, 0xe08c),
    (0xe08e, 0xe095),
    (0xe0a2, 0xe0bb),
    (0xf8f7, 0xf8f7),
];

const XTERM6_HIGH_COMBINING: &[(u32, u32)] = &[
    (0x10a01, 0x10a03),
    (0x10a05, 0x10a06),
    (0x10a0c, 0x10a0f),
    (0x10a38, 0x10a3a),
    (0x10a3f, 0x10a3f),
    (0x1d167, 0x1d169),
    (0x1d173, 0x1d182),
    (0x1d185, 0x1d18b),
    (0x1d1aa, 0x1d1ad),
    (0x1d242, 0x1d244),
    (0xe0001, 0xe0001),
    (0xe0020, 0xe007f),
    (0xe0100, 0xe01ef),
];

fn xterm_unicode6_width(codepoint: u32) -> usize {
    if codepoint < 0x20 || (0x7f..0xa0).contains(&codepoint) {
        return 0;
    }
    if codepoint < 0x7f {
        return 1;
    }
    if codepoint < 0x10000 {
        if in_sorted_ranges(codepoint, XTERM6_BMP_COMBINING) {
            return 0;
        }
        return usize::from(
            codepoint >= 0x1100
                && (codepoint <= 0x115f
                    || matches!(codepoint, 0x2329 | 0x232a)
                    || (0x2e80..=0xa4cf).contains(&codepoint) && codepoint != 0x303f
                    || (0xac00..=0xd7a3).contains(&codepoint)
                    || (0xf900..=0xfaff).contains(&codepoint)
                    || (0xfe10..=0xfe19).contains(&codepoint)
                    || (0xfe30..=0xfe6f).contains(&codepoint)
                    || (0xff00..=0xff60).contains(&codepoint)
                    || (0xffe0..=0xffe6).contains(&codepoint)),
        ) + 1;
    }
    if in_sorted_ranges(codepoint, XTERM6_HIGH_COMBINING) {
        return 0;
    }
    // The renderer draws an astral emoji in a single cell even though the
    // console reserves two for the surrogate pair. Layout follows the renderer,
    // and `console_width` below is what output has to stay under.
    if (0x20000..=0x2fffd).contains(&codepoint) || (0x30000..=0x3fffd).contains(&codepoint) {
        2
    } else {
        1
    }
}

/// Columns the console reserves for a character, which is not always what the
/// web renderer on top of it draws. Measured on this platform by writing each
/// character to a console and reading the cursor column back: the console
/// counts a surrogate pair as two columns and widens 598 ambiguous characters —
/// the middle dot and the arrows among them — that the renderer keeps narrow.
/// Layout follows the renderer so borders line up; this is what output has to
/// stay under so a row never wraps itself onto the next line.
pub(crate) fn console_width_char(ch: char) -> usize {
    let displayed = UnicodeWidthChar::width(ch).unwrap_or(0);
    if !use_devezcode_xterm_widths() {
        return displayed;
    }
    let codepoint = ch as u32;
    if codepoint >= 0x10000 {
        return 2;
    }
    if displayed == 1 && in_sorted_ranges(codepoint, CONSOLE_WIDE_AMBIGUOUS) {
        return 2;
    }
    displayed
}

pub(crate) fn console_width(text: &str) -> usize {
    text.chars().map(console_width_char).sum()
}

#[cfg(not(test))]
fn use_devezcode_xterm_widths() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        env::var("DEVEZCODE_TERM_WIDTH_PROFILE").as_deref() == Ok("xterm6-unicode6")
    })
}

#[cfg(test)]
thread_local! {
    static TEST_DEVEZCODE_XTERM_WIDTHS: TestCell<bool> = const { TestCell::new(false) };
}

#[cfg(test)]
fn use_devezcode_xterm_widths() -> bool {
    TEST_DEVEZCODE_XTERM_WIDTHS.get()
}

#[cfg(test)]
pub(crate) fn with_devezcode_xterm_widths<T>(test: impl FnOnce() -> T) -> T {
    TEST_DEVEZCODE_XTERM_WIDTHS.set(true);
    let result = test();
    TEST_DEVEZCODE_XTERM_WIDTHS.set(false);
    result
}

#[derive(Debug)]
struct TerminalFragment<'a> {
    word: &'a str,
    whitespace: &'a str,
    width: usize,
}

impl Fragment for TerminalFragment<'_> {
    fn width(&self) -> f64 {
        self.width as f64
    }

    fn whitespace_width(&self) -> f64 {
        UnicodeWidthStr::width(self.whitespace) as f64
    }

    fn penalty_width(&self) -> f64 {
        0.0
    }
}

/// `textwrap` hardcodes the modern `unicode-width` crate. Build the same
/// ASCII-space/first-fit wrapping with fragments whose widths come from the
/// active terminal profile, including break-words behavior for long paths.
pub(crate) fn wrap_ascii_space(text: &str, line_width: usize) -> Vec<Cow<'_, str>> {
    let line_width = line_width.max(1);
    let mut fragments = Vec::new();
    let mut start = 0;
    let mut in_whitespace = false;
    for (index, ch) in text.char_indices() {
        if in_whitespace && ch != ' ' {
            push_fragment_parts(&mut fragments, &text[start..index], line_width);
            start = index;
        }
        in_whitespace = ch == ' ';
    }
    if start < text.len() {
        push_fragment_parts(&mut fragments, &text[start..], line_width);
    }
    if fragments.is_empty() {
        return Vec::new();
    }

    let widths = [line_width as f64];
    let lines = wrap_optimal_fit(&fragments, &widths, &Penalties::default())
        .unwrap_or_else(|_| wrap_first_fit(&fragments, &widths));
    lines
        .into_iter()
        .map(|line| {
            let mut output = String::new();
            for (index, fragment) in line.iter().enumerate() {
                output.push_str(fragment.word);
                if index + 1 < line.len() {
                    output.push_str(fragment.whitespace);
                }
            }
            Cow::Owned(output)
        })
        .collect()
}

fn push_fragment_parts<'a>(
    output: &mut Vec<TerminalFragment<'a>>,
    fragment: &'a str,
    line_width: usize,
) {
    let word = fragment.trim_end_matches(' ');
    let whitespace = &fragment[word.len()..];
    if word.is_empty() {
        output.push(TerminalFragment {
            word,
            whitespace,
            width: 0,
        });
        return;
    }

    let mut start = 0;
    let mut width = 0;
    for (index, ch) in word.char_indices() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width > 0 && width + char_width > line_width {
            output.push(TerminalFragment {
                word: &word[start..index],
                whitespace: "",
                width,
            });
            start = index;
            width = 0;
        }
        width += char_width;
    }
    output.push(TerminalFragment {
        word: &word[start..],
        whitespace,
        width,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured on Windows by writing each character to a console and reading
    /// the cursor column back. The middle dot and arrows ride in the status
    /// line of every frame, so getting them wrong wraps the whole screen.
    #[test]
    fn console_profile_matches_the_measured_console_columns() {
        with_devezcode_xterm_widths(|| {
            for (ch, columns) in [
                ('\u{00b7}', 2),
                ('\u{2191}', 2),
                ('\u{2193}', 2),
                ('\u{2194}', 2),
                ('\u{2026}', 2),
                ('\u{2022}', 1),
                ('\u{2500}', 1),
                ('\u{2502}', 1),
                ('\u{256d}', 1),
                ('\u{276f}', 1),
                ('\u{280b}', 1),
                ('\u{23f0}', 1),
                ('\u{1f43e}', 2),
                ('가', 2),
            ] {
                assert_eq!(
                    console_width_char(ch),
                    columns,
                    "U+{:04X} takes {columns} console columns",
                    ch as u32
                );
            }
        });
    }

    #[test]
    fn paw_prints_lay_out_one_cell_each_the_way_the_renderer_draws_them() {
        with_devezcode_xterm_widths(|| {
            assert_eq!(wrap_ascii_space("🐾🐾🐾🐾🐾", 5), ["🐾🐾🐾🐾🐾"]);
            assert_eq!(console_width("🐾🐾🐾🐾🐾"), 10);
        });
    }

    #[test]
    fn modern_profile_is_unchanged() {
        assert_eq!(
            UnicodeWidthStr::width("🐾"),
            ModernUnicodeWidthStr::width("🐾")
        );
    }

    #[test]
    fn modern_profile_preserves_textwrap_optimal_layout() {
        for (text, width) in [
            ("alpha beta gamma delta epsilon", 12),
            ("긴 문장도 기존 최적 줄바꿈 배치를 유지한다", 14),
            ("https://example.com/a/very/long/path", 10),
        ] {
            let expected = textwrap::wrap(
                text,
                textwrap::Options::new(width)
                    .break_words(true)
                    .word_separator(textwrap::WordSeparator::AsciiSpace),
            )
            .into_iter()
            .map(Cow::into_owned)
            .collect::<Vec<_>>();
            let actual = wrap_ascii_space(text, width)
                .into_iter()
                .map(Cow::into_owned)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "text={text:?}, width={width}");
        }
    }
}
