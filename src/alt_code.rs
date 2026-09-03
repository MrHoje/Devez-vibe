//! The characters Windows has no key for, which arrive as an Alt code.
//!
//! ConPTY turns the text it is handed into key events. A character the keyboard
//! layout can reach, or that Windows classes as a letter — every Hangul
//! syllable and every Han character among them — gets an ordinary press and
//! release. Anything else is synthesized as an Alt code instead: numpad presses
//! under a held Alt, with the character itself riding on the Alt release. That
//! is what a Hanja conversion of a lone jamo offers (★, ※, →, ①), and crossterm
//! reports it as a single release with no press before it, so a prompt that
//! ignores releases drops the character.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

/// Turns the lone release of an Alt code back into the press it stands for.
#[derive(Debug, Default)]
pub struct AltCodeKeys {
    /// The character whose press was seen, so its own release stays a release.
    pressed: Option<char>,
}

impl AltCodeKeys {
    /// Every key event goes through here on its way to being handled.
    pub fn normalize(&mut self, key: KeyEvent) -> KeyEvent {
        let KeyCode::Char(ch) = key.code else {
            return key;
        };
        if key.kind != KeyEventKind::Release {
            self.pressed = Some(ch);
            return key;
        }
        if self.pressed.take() == Some(ch) {
            return key;
        }
        KeyEvent {
            kind: KeyEventKind::Press,
            ..key
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventState, KeyModifiers};

    use super::*;

    fn key(ch: char, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn a_release_with_no_press_before_it_becomes_a_press() {
        let mut keys = AltCodeKeys::default();
        assert_eq!(
            keys.normalize(key('★', KeyEventKind::Release)),
            key('★', KeyEventKind::Press)
        );
    }

    #[test]
    fn an_ordinary_key_keeps_its_own_release() {
        let mut keys = AltCodeKeys::default();
        assert_eq!(
            keys.normalize(key('안', KeyEventKind::Press)),
            key('안', KeyEventKind::Press)
        );
        assert_eq!(
            keys.normalize(key('안', KeyEventKind::Release)),
            key('안', KeyEventKind::Release)
        );
        // The press is spent — a second release is an Alt code of its own.
        assert_eq!(
            keys.normalize(key('안', KeyEventKind::Release)),
            key('안', KeyEventKind::Press)
        );
    }

    #[test]
    fn an_alt_code_between_two_ordinary_keys_is_the_only_one_turned_around() {
        let mut keys = AltCodeKeys::default();
        let typed = [
            key('a', KeyEventKind::Press),
            key('a', KeyEventKind::Release),
            key('※', KeyEventKind::Release),
            key('b', KeyEventKind::Press),
            key('b', KeyEventKind::Release),
        ];
        let seen: Vec<KeyEvent> = typed
            .into_iter()
            .map(|event| keys.normalize(event))
            .collect();
        assert_eq!(
            seen,
            vec![
                key('a', KeyEventKind::Press),
                key('a', KeyEventKind::Release),
                key('※', KeyEventKind::Press),
                key('b', KeyEventKind::Press),
                key('b', KeyEventKind::Release),
            ]
        );
    }

    #[test]
    fn a_key_that_is_not_a_character_passes_through() {
        let mut keys = AltCodeKeys::default();
        let enter = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        assert_eq!(keys.normalize(enter), enter);
    }
}
