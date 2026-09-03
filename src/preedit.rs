//! The syllable a Windows IME is still composing, handed over by DevezCode.
//!
//! A terminal never sees a composition in progress: the IME keeps the preedit
//! to itself and only the committed syllable reaches the PTY. DevezCode's
//! terminal paints its own preview over the cursor cell instead, which covers
//! whatever the prompt already has to the right of the cursor. Once this
//! process announces `devez-preedit-v2`, the host forwards every change of the
//! preedit as ordinary input framed by two private-use characters, so the
//! composer can draw the syllable in place and shift the rest of the prompt
//! the way the committed character will. Version 2 leaves the visual underline
//! to the host, which can place it below the terminal glyph instead of across it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

/// Opens a preedit frame. The characters up to [`PREEDIT_END`] are the preedit.
pub const PREEDIT_START: char = '\u{E000}';
/// Closes a preedit frame. An empty frame means the composition ended.
pub const PREEDIT_END: char = '\u{E001}';

/// Tells DevezCode that preedit frames are understood, so it may send them and
/// stop painting its own preview.
pub fn support_signal() -> String {
    "\x1b]777;devez-preedit-v2;1\x07".to_owned()
}

/// What a key turned out to be once the preedit frames are taken out.
#[derive(Debug, PartialEq, Eq)]
pub enum PreeditInput {
    /// An ordinary key, to be handled as before.
    Key(KeyEvent),
    /// Part of a frame; nothing to do yet.
    Swallowed,
    /// A frame closed: the preedit is now this text, empty when it ended.
    Update(String),
}

/// Collects the characters between the two frame markers.
#[derive(Debug, Default)]
pub struct PreeditCapture {
    buffer: Option<String>,
    /// The frame character whose press was counted, so its release is not.
    pressed: Option<char>,
}

impl PreeditCapture {
    /// True when this key belongs to a frame rather than to the prompt.
    pub fn claims(&self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(PREEDIT_START | PREEDIT_END) => true,
            KeyCode::Char(_) => self.buffer.is_some(),
            _ => false,
        }
    }

    pub fn observe(&mut self, key: KeyEvent) -> PreeditInput {
        let KeyCode::Char(ch) = key.code else {
            return PreeditInput::Key(key);
        };
        if !self.claims(&key) {
            return PreeditInput::Key(key);
        }
        // Windows reports a release for every press, and the press already
        // counted. A character ConPTY has no key for, and does not class as a
        // letter, arrives as an Alt code instead: a single release with no press
        // before it. The markers are such characters, so a release on its own
        // counts as the character itself.
        if key.kind == KeyEventKind::Release {
            if self.pressed.take() == Some(ch) {
                return PreeditInput::Swallowed;
            }
        } else {
            self.pressed = Some(ch);
        }
        match ch {
            PREEDIT_START => {
                self.buffer = Some(String::new());
                PreeditInput::Swallowed
            }
            PREEDIT_END => match self.buffer.take() {
                Some(text) => PreeditInput::Update(text),
                None => PreeditInput::Swallowed,
            },
            _ => {
                if let Some(buffer) = &mut self.buffer {
                    buffer.push(ch);
                }
                PreeditInput::Swallowed
            }
        }
    }
}

/// The preedit that remains once `committed` reached the prompt. A commit is
/// the text the IME was showing, so it is taken off the front; a commit that
/// does not match (a conversion, or text from elsewhere) leaves the preedit for
/// the host's next frame to settle.
pub fn preedit_after_commit(preedit: &str, committed: &str) -> Option<String> {
    if preedit.is_empty() || committed.is_empty() {
        return None;
    }
    preedit.strip_prefix(committed).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    fn key(ch: char, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn support_signal_requests_host_owned_preedit_decoration() {
        assert_eq!(support_signal(), "\x1b]777;devez-preedit-v2;1\x07");
    }

    #[test]
    fn a_frame_becomes_one_update_and_its_releases_are_dropped() {
        let mut capture = PreeditCapture::default();
        assert_eq!(
            capture.observe(key(PREEDIT_START, KeyEventKind::Press)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key(PREEDIT_START, KeyEventKind::Release)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key('ㄴ', KeyEventKind::Press)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key('ㄴ', KeyEventKind::Release)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key(PREEDIT_END, KeyEventKind::Press)),
            PreeditInput::Update("ㄴ".to_owned())
        );
        assert_eq!(
            capture.observe(key(PREEDIT_END, KeyEventKind::Release)),
            PreeditInput::Swallowed
        );
        let plain = key('a', KeyEventKind::Press);
        assert_eq!(capture.observe(plain), PreeditInput::Key(plain));
    }

    #[test]
    fn markers_delivered_as_alt_code_releases_still_frame_the_preedit() {
        // ConPTY turns the private-use markers into Alt codes: one release each,
        // no press. The Hangul in between still comes as press and release.
        let mut capture = PreeditCapture::default();
        assert_eq!(
            capture.observe(key(PREEDIT_START, KeyEventKind::Release)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key('안', KeyEventKind::Press)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key('안', KeyEventKind::Release)),
            PreeditInput::Swallowed
        );
        assert_eq!(
            capture.observe(key(PREEDIT_END, KeyEventKind::Release)),
            PreeditInput::Update("안".to_owned())
        );
        let plain = key('안', KeyEventKind::Press);
        assert_eq!(capture.observe(plain), PreeditInput::Key(plain));
    }

    #[test]
    fn an_empty_frame_ends_the_composition() {
        let mut capture = PreeditCapture::default();
        capture.observe(key(PREEDIT_START, KeyEventKind::Press));
        assert_eq!(
            capture.observe(key(PREEDIT_END, KeyEventKind::Press)),
            PreeditInput::Update(String::new())
        );
    }

    #[test]
    fn a_commit_of_the_shown_syllable_clears_the_preedit() {
        assert_eq!(preedit_after_commit("안", "안"), Some(String::new()));
        assert_eq!(preedit_after_commit("ㄴ", "안"), None);
        assert_eq!(preedit_after_commit("", "안"), None);
    }
}
