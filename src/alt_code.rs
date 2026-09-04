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

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

/// Turns the lone release of an Alt code back into the press it stands for.
#[derive(Debug, Default)]
pub struct AltCodeKeys {
    /// How many presses of each character are still held down, so each of
    /// their own releases stays a release. A single slot was enough while
    /// presses and releases strictly alternated, but fast typing overlaps
    /// them (`Press 가`, `Press 나`, `Release 가`, `Release 나`): the first
    /// release would then look like a lone Alt code and insert `가` twice.
    pressed: HashMap<char, usize>,
}

impl AltCodeKeys {
    /// Every key event goes through here on its way to being handled.
    pub fn normalize(&mut self, key: KeyEvent) -> KeyEvent {
        let KeyCode::Char(ch) = key.code else {
            return key;
        };
        if key.kind == KeyEventKind::Press {
            // A hold reports one press and then repeats, but a single release
            // closes all of them, so only presses open a hold.
            if !self.pressed.contains_key(&ch) && self.pressed.len() >= 32 {
                // Press-only terminals never send the releases that would
                // close a hold. Drop the stale holds instead of growing.
                self.pressed.clear();
            }
            *self.pressed.entry(ch).or_default() += 1;
            return key;
        }
        if key.kind == KeyEventKind::Repeat {
            return key;
        }
        if let Some(count) = self.pressed.get_mut(&ch) {
            if *count > 1 {
                *count -= 1;
            } else {
                self.pressed.remove(&ch);
            }
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

    #[test]
    fn overlapping_presses_keep_their_own_releases() {
        let mut keys = AltCodeKeys::default();
        let typed = [
            key('가', KeyEventKind::Press),
            key('나', KeyEventKind::Press),
            key('가', KeyEventKind::Release),
            key('나', KeyEventKind::Release),
        ];
        let seen: Vec<KeyEvent> = typed
            .into_iter()
            .map(|event| keys.normalize(event))
            .collect();
        assert_eq!(
            seen,
            vec![
                key('가', KeyEventKind::Press),
                key('나', KeyEventKind::Press),
                key('가', KeyEventKind::Release),
                key('나', KeyEventKind::Release),
            ]
        );
    }

    #[test]
    fn pressing_the_same_key_twice_before_either_release_keeps_both_releases() {
        let mut keys = AltCodeKeys::default();
        let typed = [
            key('가', KeyEventKind::Press),
            key('가', KeyEventKind::Press),
            key('가', KeyEventKind::Release),
            key('가', KeyEventKind::Release),
        ];
        let seen: Vec<KeyEvent> = typed
            .into_iter()
            .map(|event| keys.normalize(event))
            .collect();
        assert_eq!(
            seen,
            vec![
                key('가', KeyEventKind::Press),
                key('가', KeyEventKind::Press),
                key('가', KeyEventKind::Release),
                key('가', KeyEventKind::Release),
            ]
        );
    }

    #[test]
    fn repeats_do_not_open_an_extra_hold() {
        let mut keys = AltCodeKeys::default();
        assert_eq!(
            keys.normalize(key('가', KeyEventKind::Press)),
            key('가', KeyEventKind::Press)
        );
        assert_eq!(
            keys.normalize(key('가', KeyEventKind::Repeat)),
            key('가', KeyEventKind::Repeat)
        );
        assert_eq!(
            keys.normalize(key('가', KeyEventKind::Release)),
            key('가', KeyEventKind::Release)
        );
        assert_eq!(
            keys.normalize(key('가', KeyEventKind::Release)),
            key('가', KeyEventKind::Press)
        );
    }

    #[test]
    fn press_only_terminals_do_not_accumulate_holds() {
        let mut keys = AltCodeKeys::default();
        for ch in ('가'..='힣').take(40) {
            assert_eq!(
                keys.normalize(key(ch, KeyEventKind::Press)),
                key(ch, KeyEventKind::Press)
            );
        }
        assert!(
            keys.pressed.len() <= 32,
            "stale holds stay bounded without releases"
        );
        assert_eq!(
            keys.normalize(key('★', KeyEventKind::Release)),
            key('★', KeyEventKind::Press)
        );
    }
}
