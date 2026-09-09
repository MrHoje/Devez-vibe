//! 실제 셀 크기가 확인된 터미널에서만 질문 모서리 한 칸을 Sixel로 보정한다.
use std::sync::atomic::{AtomicU32, Ordering};

use crate::theme::Rgb;

static CELL_SIZE: AtomicU32 = AtomicU32::new(0);

pub fn set_size(width: u16, height: u16) {
    let value = if (2..=256).contains(&width) && (2..=512).contains(&height) {
        u32::from(width) | (u32::from(height) << 16)
    } else {
        0
    };
    CELL_SIZE.store(value, Ordering::Relaxed);
}

pub fn size() -> Option<(u16, u16)> {
    let value = CELL_SIZE.load(Ordering::Relaxed);
    (value != 0).then_some((value as u16, (value >> 16) as u16))
}

fn pixel_color(x: u16, y: u16, width: u16, height: u16, top: bool) -> u8 {
    let inside = if top { y >= height / 2 } else { y < height / 2 };
    if !inside {
        0
    } else if x < width / 2 {
        2
    } else {
        1
    }
}

pub fn corner(
    width: u16,
    height: u16,
    top: bool,
    line: Rgb,
    fill: Rgb,
    outside: Rgb,
) -> Option<String> {
    use std::fmt::Write;
    if !(2..=256).contains(&width) || !(2..=512).contains(&height) {
        return None;
    }
    let mut data = format!("\x1bP0;1;0q\"1;1;{width};{height}");
    for (index, color) in [outside, fill, line].iter().enumerate() {
        let percent = |value: u8| (u32::from(value) * 100 + 127) / 255;
        write!(
            data,
            "#{index};2;{};{};{}",
            percent(color.0),
            percent(color.1),
            percent(color.2)
        )
        .unwrap();
    }
    for band in (0..height).step_by(6) {
        for color in 0..3 {
            write!(data, "#{color}").unwrap();
            let mask = |x| {
                let mut bits = 0u8;
                for offset in 0..6 {
                    if band + offset < height
                        && pixel_color(x, band + offset, width, height, top) == color
                    {
                        bits |= 1 << offset;
                    }
                }
                (63 + bits) as char
            };
            let mut x = 0;
            while x < width {
                let glyph = mask(x);
                let start = x;
                while x < width && mask(x) == glyph {
                    x += 1;
                }
                write!(data, "!{}{glyph}", x - start).unwrap();
            }
            data.push('$');
        }
        if band + 6 < height {
            data.push('-');
        }
    }
    data.push_str("\x1b\\");
    Some(data)
}

/// 반환 범위는 문자 단위다. 질문 응답에 섞인 실제 키 이벤트를 원래 순서로 돌려주기 위해 쓴다.
pub fn report(
    chars: &[char],
    prefix: &str,
    final_char: char,
) -> Option<(std::ops::Range<usize>, Vec<u16>)> {
    let prefix = prefix.chars().collect::<Vec<_>>();
    for start in 0..chars.len() {
        // Windows의 키 이벤트 변환은 VK가 없는 응답의 ESC를 생략할 수 있다.
        let matched = if chars[start..].starts_with(&prefix) {
            prefix.len()
        } else if prefix.first() == Some(&'\x1b') && chars[start..].starts_with(&prefix[1..]) {
            prefix.len() - 1
        } else {
            continue;
        };
        let body = start + matched;
        let end = chars[body..].iter().position(|&ch| ch == final_char)? + body;
        // 명시적으로 요청한 보고서가 잘못되어도 그 숫자를 사용자 선택 키로 돌려보내지 않는다.
        let values = if end - body <= 96
            && chars[body..end]
                .iter()
                .all(|ch| ch.is_ascii_digit() || *ch == ';')
        {
            chars[body..end]
                .iter()
                .collect::<String>()
                .split(';')
                .map(str::parse::<u16>)
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        return Some((start..end + 1, values));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_reports_preserve_surrounding_korean_input() {
        let chars = "앞\x1b[6;20;10t뒤".chars().collect::<Vec<_>>();
        let (range, values) = report(&chars, "\x1b[6;", 't').unwrap();
        assert_eq!(values, vec![20, 10]);
        assert_eq!(chars[..range.start].iter().collect::<String>(), "앞");
        assert_eq!(chars[range.end..].iter().collect::<String>(), "뒤");
        assert!(
            report(
                &"\x1b[6;999999;10t".chars().collect::<Vec<_>>(),
                "\x1b[6;",
                't'
            )
            .unwrap()
            .1
            .is_empty()
        );
        assert!(
            report(
                &"\x1b[6;abc;10t".chars().collect::<Vec<_>>(),
                "\x1b[6;",
                't'
            )
            .unwrap()
            .1
            .is_empty()
        );
        assert_eq!(
            report(&"\x1b[?61;4;6c".chars().collect::<Vec<_>>(), "\x1b[?", 'c')
                .unwrap()
                .1,
            vec![61, 4, 6]
        );
        assert_eq!(
            report(&"[6;20;10t".chars().collect::<Vec<_>>(), "\x1b[6;", 't')
                .unwrap()
                .1,
            vec![20, 10]
        );
        assert_eq!(
            report(&"[?61;4;6c".chars().collect::<Vec<_>>(), "\x1b[?", 'c')
                .unwrap()
                .1,
            vec![61, 4, 6]
        );
    }

    #[test]
    fn sixel_corner_has_three_regions_without_a_protruding_background() {
        for top in [true, false] {
            assert_eq!(pixel_color(0, 0, 10, 20, top), if top { 0 } else { 2 });
            assert_eq!(pixel_color(9, 0, 10, 20, top), if top { 0 } else { 1 });
            assert_eq!(pixel_color(0, 19, 10, 20, top), if top { 2 } else { 0 });
            assert_eq!(pixel_color(9, 19, 10, 20, top), if top { 1 } else { 0 });
            let data = corner(
                10,
                20,
                top,
                Rgb(52, 211, 199),
                Rgb(54, 54, 54),
                Rgb(31, 31, 30),
            )
            .unwrap();
            assert!(data.starts_with("\x1bP0;1;0q\"1;1;10;20"));
            assert!(data.ends_with("\x1b\\"));
            assert!(data.len() < 512);
        }
        assert!(corner(0, 20, true, Rgb(0, 0, 0), Rgb(0, 0, 0), Rgb(0, 0, 0)).is_none());
        assert!(corner(257, 20, true, Rgb(0, 0, 0), Rgb(0, 0, 0), Rgb(0, 0, 0)).is_none());
    }

    #[tokio::test]
    #[ignore = "Windows Terminal에서 별도 시험 프로세스로 실행 필요"]
    async fn live_native_terminal_metrics() {
        crossterm::terminal::enable_raw_mode().unwrap();
        crate::input_hub::install();
        crate::input_hub::query_graphics().await;
        let pixels = size();
        crossterm::terminal::disable_raw_mode().unwrap();
        if let Ok(path) = std::env::var("DEVEZ_GRAPHICS_TEST_RESULT") {
            std::fs::write(path, serde_json::to_vec(&pixels).unwrap()).unwrap();
        }
        assert!(pixels.is_some());
    }
}
