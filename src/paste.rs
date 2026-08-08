//! Rescues pasted newlines on Windows, where bracketed paste never arrives.
//!
//! Bracketed paste is a terminal-side feature: the emulator wraps pasted text
//! in `ESC[200~ … ESC[201~` so the application can tell it apart from typing.
//! Three things were measured on this terminal, and together they close every
//! door but this one:
//!
//! - crossterm reads console records on Windows, not the byte stream, and
//!   `EnableBracketedPaste` is unimplemented there. The markers never show up.
//! - The console records carry no mark of their own. Windows Terminal
//!   synthesizes a paste with `VkKeyScan`, so a pasted `\r` is indistinguishable
//!   from a typed one: both are `vk=13 scan=28`.
//! - The markers *do* arrive under `ENABLE_VIRTUAL_TERMINAL_INPUT`, but that
//!   mode also stops crossterm from decoding anything — arrows come through as
//!   `Esc [ A`, Enter as a bare `\r`. Reaching the markers means writing the
//!   whole VT decoder, mouse included.
//!
//! So timing is the only signal left, the same conclusion Codex CLI reached
//! (`tui/src/bottom_pane/paste_burst.rs`, with a `disable_paste_burst` escape
//! hatch). What matters is *which* timing. The gap before `Enter` is useless:
//! a Hangul IME commits the syllable being composed at the moment Enter is
//! pressed, so a typed Enter arrives in the same batch as the character before
//! it, 0ms behind — exactly like a pasted one.
//!
//! Measured on this terminal (`cargo run --example keylog`):
//!
//! ```text
//! typed:  가 -   나 222  다 192  라 155  마 800  ENTER 0
//! pasted: 첫 13780  째 0  줄 0  ` 0  n 0  둘 0  …  (13 in a row)
//! ```
//!
//! The separation is not in the last gap, it is in how many characters arrived
//! fast *before* it. Typing produces none — every gap but the IME's is over
//! 100ms. A paste produces an unbroken run. That run is what this counts.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Below this, a character did not come from a finger. The slowest paste gap
/// measured was 0ms and the fastest typing gap 123ms, so this sits in open
/// space between them — high enough to absorb the redraw that happens between
/// two key events in the real application, low enough that reaching it by
/// typing would take about 2,400 characters per minute.
const FAST_GAP: Duration = Duration::from_millis(16);

/// `Ctrl+V` is an explicit paste signal even when Windows delivers the payload
/// as ordinary key records. Keep that transaction open across short scheduler
/// gaps so a payload newline can never escape as a submit key.
const SHORTCUT_PASTE_GAP: Duration = Duration::from_millis(250);

/// Once incoming keys match the collapsed block already in the editor, they
/// are a second paste even when Windows Terminal never forwards `Ctrl+V`.
const MATCHED_PASTE_GAP: Duration = Duration::from_millis(250);

/// A run the clipboard itself vouched for is a paste no matter how slowly the
/// terminal feeds it, so this gap only has to be short enough that a person who
/// typed the same prefix and then stopped still sees their characters appear.
const VERIFIED_PASTE_GAP: Duration = Duration::from_millis(2_000);

/// How many fast characters have to pile up before `Enter` stops meaning send.
/// Typing scores zero, because the one 0ms gap an IME produces belongs to the
/// Enter itself and never to a character. Two is therefore already decisive,
/// and keeping it low is what lets a paste whose first line is short still be
/// caught.
const MIN_RUN: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BufferedTextTarget {
    #[default]
    Composer,
    PendingUserInput(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedText {
    pub text: String,
    pub pasted: bool,
    pub target: BufferedTextTarget,
}

#[derive(Debug)]
pub enum ComposerInput {
    Key(KeyEvent),
    Text(BufferedText),
}

/// Holds a short run of characters before it reaches the composer. On Windows
/// that is the only way to turn a paste-produced image path into an attachment
/// before the raw path is ever rendered.
#[derive(Debug, Default)]
pub struct ComposerPasteBuffer {
    last: Option<Instant>,
    text: String,
    pasted: bool,
    target: BufferedTextTarget,
    shortcut_paste: bool,
    expected_paste: Option<Vec<char>>,
    expected_index: usize,
    /// Set when the clipboard was read and matched, so the run in progress is a
    /// paste by evidence rather than by timing.
    verified_paste: bool,
    discard: Option<Vec<char>>,
    discard_index: usize,
    disabled: bool,
}

impl ComposerPasteBuffer {
    pub fn new() -> Self {
        Self {
            disabled: std::env::var_os("DVZ_DISABLE_PASTE_BURST").is_some(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn observe(&mut self, key: KeyEvent, now: Instant) -> Vec<ComposerInput> {
        self.observe_expected(key, now, None)
    }

    /// Keeps the short buffered run bound to the input that owned its first key.
    /// If focus changes before another key arrives, the prior run is released
    /// with its original target before the new target starts receiving input.
    pub fn observe_targeted(
        &mut self,
        key: KeyEvent,
        now: Instant,
        target: BufferedTextTarget,
    ) -> Vec<ComposerInput> {
        self.observe_expected_targeted(key, now, None, target)
    }

    pub fn observe_expected(
        &mut self,
        key: KeyEvent,
        now: Instant,
        expected_paste: Option<&str>,
    ) -> Vec<ComposerInput> {
        self.observe_expected_targeted(
            key,
            now,
            expected_paste,
            BufferedTextTarget::Composer,
        )
    }

    fn observe_expected_targeted(
        &mut self,
        key: KeyEvent,
        now: Instant,
        expected_paste: Option<&str>,
        target: BufferedTextTarget,
    ) -> Vec<ComposerInput> {
        let mut prior = if !self.text.is_empty() && self.target != target {
            self.flush().into_iter().map(ComposerInput::Text).collect()
        } else {
            Vec::new()
        };
        if self.text.is_empty() {
            self.target = target;
        }
        prior.extend(self.observe_expected_same_target(key, now, expected_paste));
        prior
    }

    fn observe_expected_same_target(
        &mut self,
        key: KeyEvent,
        now: Instant,
        expected_paste: Option<&str>,
    ) -> Vec<ComposerInput> {
        if self.disabled {
            return vec![ComposerInput::Key(key)];
        }
        if !matches!(key.kind, KeyEventKind::Press) {
            return Vec::new();
        }
        if matches!(key.code, KeyCode::Char('v' | 'V'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let pending = self.flush();
            self.shortcut_paste = true;
            self.last = Some(now);
            return pending.into_iter().map(ComposerInput::Text).collect();
        }
        if let Some(expected) = &self.discard {
            let key_char = paste_key_char(&key, true);
            if key_char.is_some_and(|ch| expected.get(self.discard_index) == Some(&ch)) {
                self.discard_index += 1;
                self.last = Some(now);
                if self.discard_index == expected.len() {
                    self.discard = None;
                    self.discard_index = 0;
                    // Whatever the terminal still appends belongs to the same
                    // paste, so it must not reach the composer as a submit key.
                    self.shortcut_paste = true;
                }
                return Vec::new();
            }
            // Not the payload after all. It is input, but it arrives inside the
            // open paste transaction so a stray newline cannot send the prompt.
            self.discard = None;
            self.discard_index = 0;
            self.shortcut_paste = true;
        }

        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();

        if let Some(expected) = &self.expected_paste {
            let key_char = paste_key_char(&key, true);
            if key_char.is_some_and(|ch| expected.get(self.expected_index) == Some(&ch)) {
                let ch = key_char.expect("matched paste key has text");
                self.text.push(ch);
                self.expected_index += 1;
                // A verified run is only a paste once it completes; until then
                // it could still be someone typing the same characters, and
                // that must reach the composer as typing.
                self.pasted |= !self.verified_paste;
                self.last = Some(now);
                if self.expected_index == expected.len() {
                    self.pasted = true;
                    let matched = self.flush();
                    // The block is complete, but a terminal that appends the
                    // clipboard's trailing line ending is still inside this
                    // paste. Keeping the transaction open is what stops that
                    // last Enter from sending the prompt.
                    self.shortcut_paste = true;
                    self.last = Some(now);
                    return matched.into_iter().map(ComposerInput::Text).collect();
                }
                return Vec::new();
            }
            self.expected_paste = None;
            self.expected_index = 0;
            if self.verified_paste {
                // The run stopped reproducing the clipboard, so it was typing
                // after all. Release what was held as typed text and let this
                // key act normally — an Enter here still sends.
                return self
                    .flush()
                    .into_iter()
                    .map(ComposerInput::Text)
                    .chain(std::iter::once(ComposerInput::Key(key)))
                    .collect();
            }
        }

        if self.text.is_empty()
            && let (Some(expected), Some(ch)) = (expected_paste, paste_key_char(&key, plain))
        {
            // Line endings reach us as `Enter`, never as a carriage return, so a
            // block pasted with CRLF still has to match the keys behind it.
            let expected = paste_payload_chars(expected);
            if expected.first() == Some(&ch) {
                self.text.push(ch);
                self.last = Some(now);
                self.expected_index = 1;
                self.expected_paste = Some(expected);
                return Vec::new();
            }
        }

        let fast = self
            .last
            .is_some_and(|last| now.duration_since(last) < self.idle_gap());

        match key.code {
            // Shift+Space folds the plan panel, so it is a shortcut rather than
            // typed text. Buffering it as a space would swallow the chord before
            // the composer ever sees it.
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => self
                .flush()
                .into_iter()
                .map(ComposerInput::Text)
                .chain(std::iter::once(ComposerInput::Key(key)))
                .collect(),
            KeyCode::Char(ch) if plain => self.push_char(ch, now, fast),
            KeyCode::Enter if plain && (self.shortcut_paste || self.pasted && fast) => {
                self.text.push('\n');
                self.pasted = true;
                self.last = Some(now);
                Vec::new()
            }
            KeyCode::Tab if plain && (self.shortcut_paste || !self.text.is_empty() && fast) => {
                self.text.push('\t');
                self.pasted = true;
                self.last = Some(now);
                Vec::new()
            }
            _ => self
                .flush()
                .into_iter()
                .map(ComposerInput::Text)
                .chain(std::iter::once(ComposerInput::Key(key)))
                .collect(),
        }
    }

    /// Announces a paste the clipboard has already vouched for: the keys that
    /// follow reproduce `text`. Nothing about the run is then a guess, so no
    /// gap between its keys can let a payload newline out as a submit key.
    pub fn expect_verified_paste(&mut self, text: &str, now: Instant) {
        self.flush();
        self.target = BufferedTextTarget::Composer;
        self.expected_paste = Some(paste_payload_chars(text));
        self.expected_index = 0;
        self.verified_paste = true;
        self.last = Some(now);
    }

    /// Swallows the payload a paste shortcut is about to deliver. The composer
    /// already acted on the clipboard itself, so the keys Windows synthesizes
    /// behind `Ctrl+V` must not reach it a second time.
    pub fn discard_expected(&mut self, text: &str, now: Instant) {
        self.flush();
        self.discard = Some(paste_payload_chars(text));
        self.discard_index = 0;
        self.last = Some(now);
    }

    /// Answers for an `Event::Paste` carrying the payload already being
    /// swallowed, so a terminal that sends both the shortcut and the bracketed
    /// paste never applies it twice.
    pub fn take_discarded_paste(&mut self, text: &str) -> bool {
        if self
            .discard
            .as_ref()
            .is_none_or(|expected| paste_payload_chars(text) != *expected)
        {
            return false;
        }
        self.discard = None;
        self.discard_index = 0;
        self.last = None;
        true
    }

    pub fn flush_if_idle(&mut self, now: Instant) -> Option<BufferedText> {
        if !self
            .last
            .is_some_and(|last| now.duration_since(last) >= self.idle_gap())
        {
            return None;
        }
        self.flush()
    }

    /// While text is waiting to be classified, repainting for every key would
    /// turn a large Windows paste into one expensive frame per character.
    pub fn is_buffering(&self) -> bool {
        self.shortcut_paste || !self.text.is_empty() || self.discard.is_some()
    }

    /// The event loop sleeps until this instant instead of waking every 5ms
    /// while no paste is being classified.
    pub fn flush_deadline(&self) -> Option<Instant> {
        self.is_buffering()
            .then_some(self.last)
            .flatten()
            .map(|last| last + self.idle_gap())
    }

    fn push_char(&mut self, ch: char, now: Instant, fast: bool) -> Vec<ComposerInput> {
        if self.shortcut_paste {
            self.text.push(ch);
            self.pasted = true;
            self.last = Some(now);
            return Vec::new();
        }
        if self.text.is_empty() {
            self.text.push(ch);
            self.last = Some(now);
            return Vec::new();
        }
        if fast {
            self.text.push(ch);
            self.pasted = true;
            self.last = Some(now);
            return Vec::new();
        }
        let prior = self.flush().expect("non-empty text flushes");
        self.text.push(ch);
        self.last = Some(now);
        vec![ComposerInput::Text(prior)]
    }

    fn flush(&mut self) -> Option<BufferedText> {
        let buffered = (!self.text.is_empty()).then(|| BufferedText {
            text: std::mem::take(&mut self.text),
            pasted: std::mem::take(&mut self.pasted),
            target: self.target.clone(),
        });
        self.last = None;
        self.shortcut_paste = false;
        self.expected_paste = None;
        self.expected_index = 0;
        self.verified_paste = false;
        self.discard = None;
        self.discard_index = 0;
        buffered
    }

    fn idle_gap(&self) -> Duration {
        if self.verified_paste {
            VERIFIED_PASTE_GAP
        } else if self.discard.is_some() || self.expected_paste.is_some() {
            MATCHED_PASTE_GAP
        } else if self.shortcut_paste {
            SHORTCUT_PASTE_GAP
        } else {
            FAST_GAP
        }
    }
}

/// The payload as key records carry it: a terminal turns every line ending into
/// one `Enter`, so the carriage returns a clipboard holds have no key of their own.
pub fn paste_payload_chars(text: &str) -> Vec<char> {
    text.chars().filter(|&ch| ch != '\r').collect()
}

/// The character a key would contribute to a paste payload, if any. Shift is
/// what a capital letter arrives with, so it does not disqualify the key.
pub fn payload_char(key: &KeyEvent) -> Option<char> {
    if !matches!(key.kind, KeyEventKind::Press) {
        return None;
    }
    paste_key_char(key, key.modifiers.difference(KeyModifiers::SHIFT).is_empty())
}

fn paste_key_char(key: &KeyEvent, plain: bool) -> Option<char> {
    if !plain {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(ch),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// Counts how many characters in a row arrived faster than a person types.
#[derive(Debug, Default)]
pub struct PasteBurst {
    /// When the last key that could be part of pasted text was seen.
    last: Option<Instant>,
    /// Length of the current run of fast characters.
    run: usize,
    /// Set once by the environment, so a user whose terminal defeats this can
    /// turn it off the way Codex lets them.
    disabled: bool,
}

impl PasteBurst {
    pub fn new() -> Self {
        Self {
            disabled: std::env::var_os("DVZ_DISABLE_PASTE_BURST").is_some(),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        !self.disabled && self.run >= MIN_RUN
    }

    /// Reads `key` and hands back the key the application should act on.
    ///
    /// Only `Enter` and `Tab` are ever rewritten, and only mid-burst: `Enter`
    /// to the `Shift+Enter` the composer already reads as "insert a newline",
    /// `Tab` to the literal character it is inside pasted code. Both aim at one
    /// target — pasted text landing exactly as `Editor::insert_str` would have
    /// laid it down had a real `Event::Paste` arrived. `now` is a parameter so
    /// the run can be driven by a test clock.
    pub fn observe(&mut self, key: KeyEvent, now: Instant) -> KeyEvent {
        if self.disabled {
            return key;
        }
        // Releases and repeats carry no timing of their own; leaving the run
        // untouched keeps a key-release-reporting terminal from breaking it.
        if !matches!(key.kind, KeyEventKind::Press) {
            return key;
        }
        // Shift is what a capital letter arrives with, so it cannot disqualify
        // a keystroke from being pasted text.
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let fast = self
            .last
            .is_some_and(|last| now.duration_since(last) < FAST_GAP);

        match key.code {
            KeyCode::Char(_) if plain => {
                self.run = if fast { self.run + 1 } else { 1 };
                self.last = Some(now);
                key
            }
            KeyCode::Enter if plain && fast && self.run >= MIN_RUN => {
                // The newline is part of the run, so the next line's characters
                // keep counting from here and the paste stays one burst.
                self.run += 1;
                self.last = Some(now);
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
            }
            KeyCode::Tab if plain && fast && self.run >= MIN_RUN => {
                self.run += 1;
                self.last = Some(now);
                KeyEvent::new(KeyCode::Char('\t'), KeyModifiers::NONE)
            }
            _ => {
                self.run = 0;
                self.last = None;
                key
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Replays a measured sequence of `(gap_ms, key)` and reports what the
    /// composer would have been handed.
    fn replay(events: &[(u64, KeyCode)]) -> Vec<KeyEvent> {
        let base = Instant::now();
        let mut burst = PasteBurst::new();
        let mut at = base;
        events
            .iter()
            .map(|(gap, code)| {
                at += Duration::from_millis(*gap);
                burst.observe(press(*code), at)
            })
            .collect()
    }

    fn submits(key: &KeyEvent) -> bool {
        key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE
    }

    #[test]
    fn a_typed_hangul_sentence_still_submits() {
        // Verbatim from `cargo run --example keylog`. The trailing 0ms is the
        // IME committing 마 as Enter is pressed — the gap that made a plain
        // threshold impossible.
        let keys = replay(&[
            (0, KeyCode::Char('가')),
            (222, KeyCode::Char('나')),
            (192, KeyCode::Char('다')),
            (155, KeyCode::Char('라')),
            (800, KeyCode::Char('마')),
            (0, KeyCode::Enter),
        ]);
        assert!(
            submits(keys.last().unwrap()),
            "typing produces no run of fast characters, so Enter still sends"
        );
    }

    #[test]
    fn a_pasted_run_turns_enter_into_a_newline() {
        // Also from the probe: the first character lands after a long pause,
        // and everything behind it arrives in the same batch.
        let keys = replay(&[
            (13780, KeyCode::Char('첫')),
            (0, KeyCode::Char('째')),
            (0, KeyCode::Char('줄')),
            (0, KeyCode::Enter),
            (0, KeyCode::Char('둘')),
            (0, KeyCode::Char('째')),
            (0, KeyCode::Char('줄')),
            (0, KeyCode::Enter),
        ]);
        assert!(
            keys.iter().filter(|key| submits(key)).count() == 0,
            "no line of a paste may submit"
        );
        assert_eq!(
            keys.iter()
                .filter(|key| key.code == KeyCode::Enter && key.modifiers == KeyModifiers::SHIFT)
                .count(),
            2
        );
    }

    #[test]
    fn a_two_character_first_line_of_a_paste_does_not_submit() {
        let keys = replay(&[
            (5000, KeyCode::Char('첫')),
            (0, KeyCode::Char('줄')),
            (0, KeyCode::Enter),
        ]);

        assert!(
            !submits(keys.last().unwrap()),
            "a two-character first line is still a pasted newline"
        );
    }

    #[test]
    fn fast_character_run_is_reported_as_a_paste() {
        let base = Instant::now();
        let mut burst = PasteBurst::new();
        burst.observe(press(KeyCode::Char('a')), base);
        burst.observe(press(KeyCode::Char('b')), base + Duration::from_millis(1));
        burst.observe(press(KeyCode::Char('c')), base + Duration::from_millis(2));

        assert!(burst.is_active());
    }

    #[test]
    fn composer_buffer_releases_a_fast_path_as_one_paste() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        let mut at = base;
        for ch in r"C:\Temp\clipboard.png".chars() {
            buffer.observe(press(KeyCode::Char(ch)), at);
            at += Duration::from_millis(1);
        }

        assert_eq!(
            buffer.flush_if_idle(at + Duration::from_millis(FAST_GAP.as_millis() as u64)),
            Some(BufferedText {
                text: r"C:\Temp\clipboard.png".to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn composer_buffer_does_not_mark_a_slowly_typed_path_as_paste() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.observe(press(KeyCode::Char('C')), base);

        assert_eq!(
            buffer.flush_if_idle(base + FAST_GAP),
            Some(BufferedText {
                text: "C".to_owned(),
                pasted: false,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn composer_buffer_sends_enter_after_one_fast_character() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.observe(press(KeyCode::Char('가')), base);

        let inputs = buffer.observe(press(KeyCode::Enter), base);

        assert!(matches!(
            &inputs[0],
            ComposerInput::Text(BufferedText { text, pasted: false, .. }) if text == "가"
        ));
        assert!(matches!(
            &inputs[1],
            ComposerInput::Key(key) if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE
        ));
    }

    #[test]
    fn composer_buffer_batches_a_hangul_commit_with_ctrl_backspace() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        assert!(buffer.observe(press(KeyCode::Char('라')), base).is_empty());

        let inputs = buffer.observe(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
            base + Duration::from_millis(1),
        );
        assert!(matches!(
            &inputs[0],
            ComposerInput::Text(BufferedText { text, pasted: false, .. }) if text == "라"
        ));
        assert!(matches!(
            &inputs[1],
            ComposerInput::Key(key)
                if key.code == KeyCode::Backspace
                    && key.modifiers == KeyModifiers::CONTROL
        ));
        assert!(!buffer.is_buffering());
    }

    #[test]
    fn composer_buffer_keeps_enter_in_a_confirmed_paste_burst() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.observe(press(KeyCode::Char('첫')), base);
        buffer.observe(press(KeyCode::Char('줄')), base + Duration::from_millis(1));

        assert!(
            buffer
                .observe(press(KeyCode::Enter), base + Duration::from_millis(2))
                .is_empty()
        );
        assert_eq!(
            buffer.flush_if_idle(base + FAST_GAP + Duration::from_millis(2)),
            Some(BufferedText {
                text: "첫줄\n".to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn ctrl_v_keeps_a_slow_short_line_and_newline_in_one_paste() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        assert!(
            buffer
                .observe(
                    KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                    base,
                )
                .is_empty()
        );
        assert!(
            buffer
                .observe(press(KeyCode::Char('가')), base + Duration::from_millis(40),)
                .is_empty()
        );
        assert!(
            buffer
                .observe(press(KeyCode::Enter), base + Duration::from_millis(80))
                .is_empty()
        );
        assert!(
            buffer
                .observe(
                    press(KeyCode::Char('나')),
                    base + Duration::from_millis(120),
                )
                .is_empty()
        );

        assert_eq!(
            buffer.flush_if_idle(base + Duration::from_millis(370)),
            Some(BufferedText {
                text: "가\n나".to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn matched_second_paste_keeps_modified_symbols_and_newlines_together() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        let expected = "a@\nb";

        assert!(
            buffer
                .observe_expected(press(KeyCode::Char('a')), base, Some(expected))
                .is_empty()
        );
        assert!(
            buffer
                .observe_expected(
                    KeyEvent::new(
                        KeyCode::Char('@'),
                        KeyModifiers::CONTROL | KeyModifiers::ALT,
                    ),
                    base + Duration::from_millis(40),
                    Some(expected),
                )
                .is_empty()
        );
        assert!(
            buffer
                .observe_expected(
                    press(KeyCode::Enter),
                    base + Duration::from_millis(80),
                    Some(expected),
                )
                .is_empty()
        );

        let inputs = buffer.observe_expected(
            press(KeyCode::Char('b')),
            base + Duration::from_millis(120),
            Some(expected),
        );
        assert!(matches!(
            &inputs[..],
            [ComposerInput::Text(BufferedText { text, pasted: true, .. })]
                if text == expected
        ));
    }

    #[test]
    fn a_verified_paste_survives_a_slow_terminal_and_never_submits() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        // Half a second between keys: far outside every burst gap, and exactly
        // what a host that feeds the payload a record at a time produces.
        buffer.expect_verified_paste("a\r\nb\r\nc", base);

        let mut at = base;
        let mut inputs = Vec::new();
        for code in [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Char('b'),
            KeyCode::Enter,
            KeyCode::Char('c'),
        ] {
            at += Duration::from_millis(500);
            assert!(
                buffer.flush_if_idle(at - Duration::from_millis(1)).is_none(),
                "the run is not released while the clipboard still explains it"
            );
            inputs.extend(buffer.observe(press(code), at));
        }

        assert!(matches!(
            &inputs[..],
            [ComposerInput::Text(BufferedText { text, pasted: true, .. })] if text == "a\nb\nc"
        ));
    }

    #[test]
    fn typing_that_leaves_a_verified_paste_behind_still_submits() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.expect_verified_paste("abc", base);
        assert!(
            buffer
                .observe(press(KeyCode::Char('a')), base + Duration::from_millis(200))
                .is_empty()
        );

        let inputs = buffer.observe(press(KeyCode::Enter), base + Duration::from_millis(400));

        assert!(matches!(
            &inputs[0],
            ComposerInput::Text(BufferedText { text, pasted: false, .. }) if text == "a"
        ));
        assert!(matches!(
            &inputs[1],
            ComposerInput::Key(key)
                if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE
        ));
    }

    #[test]
    fn a_verified_prefix_that_stops_is_released_as_typing() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.expect_verified_paste("abc", base);
        buffer.observe(press(KeyCode::Char('a')), base);

        assert_eq!(
            buffer.flush_if_idle(base + VERIFIED_PASTE_GAP),
            Some(BufferedText {
                text: "a".to_owned(),
                pasted: false,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn a_discarded_payload_never_reaches_the_composer_or_submits() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        // The clipboard holds CRLF; the terminal delivers one Enter per line.
        buffer.discard_expected("a\r\nb", base);

        let mut at = base;
        for code in [KeyCode::Char('a'), KeyCode::Enter, KeyCode::Char('b')] {
            at += Duration::from_millis(1);
            assert!(buffer.observe(press(code), at).is_empty());
        }

        // A terminal that appends the line ending must not send the prompt.
        at += Duration::from_millis(1);
        assert!(buffer.observe(press(KeyCode::Enter), at).is_empty());
    }

    #[test]
    fn a_bracketed_paste_of_the_discarded_payload_is_taken_once() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.discard_expected("a\r\nb", base);

        assert!(buffer.take_discarded_paste("a\r\nb"));
        assert!(!buffer.take_discarded_paste("a\r\nb"), "only the first one");
        assert!(!buffer.is_buffering());
    }

    #[test]
    fn typing_through_a_discarded_payload_is_kept_as_input() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        buffer.discard_expected("abc", base);
        buffer.observe(press(KeyCode::Char('a')), base + Duration::from_millis(1));

        // 'x' is not the payload, so the discard ends and the key becomes input.
        assert!(
            buffer
                .observe(press(KeyCode::Char('x')), base + Duration::from_millis(2))
                .is_empty()
        );

        assert_eq!(
            buffer.flush_if_idle(base + Duration::from_millis(2) + MATCHED_PASTE_GAP),
            Some(BufferedText {
                text: "x".to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            })
        );
    }

    #[test]
    fn composer_buffer_only_arms_a_flush_deadline_while_text_waits() {
        let base = Instant::now();
        let mut buffer = ComposerPasteBuffer::new();
        assert_eq!(buffer.flush_deadline(), None);

        buffer.observe(press(KeyCode::Char('a')), base);
        assert_eq!(buffer.flush_deadline(), Some(base + FAST_GAP));

        assert!(buffer.flush_if_idle(base + FAST_GAP).is_some());
        assert_eq!(buffer.flush_deadline(), None);
    }

    #[test]
    fn a_paste_arriving_through_a_redraw_is_still_caught() {
        // The probe barely paints; the application redraws between every key.
        // A few milliseconds of that has to stay under the threshold.
        let keys = replay(&[
            (5000, KeyCode::Char('a')),
            (6, KeyCode::Char('b')),
            (7, KeyCode::Char('c')),
            (6, KeyCode::Enter),
        ]);
        assert!(!submits(keys.last().unwrap()));
    }

    #[test]
    fn a_single_character_before_enter_is_not_a_paste() {
        // A single character before Enter is what committing a composition can
        // look like, not enough evidence that text was pasted.
        let keys = replay(&[(200, KeyCode::Char('a')), (0, KeyCode::Enter)]);
        assert!(
            submits(keys.last().unwrap()),
            "the run is one character short of MIN_RUN"
        );
    }

    #[test]
    fn a_pause_inside_a_paste_ends_the_burst() {
        let keys = replay(&[
            (5000, KeyCode::Char('a')),
            (0, KeyCode::Char('b')),
            (0, KeyCode::Char('c')),
            (400, KeyCode::Enter),
        ]);
        assert!(submits(keys.last().unwrap()));
    }

    #[test]
    fn pasted_tab_becomes_a_literal_tab() {
        let keys = replay(&[
            (5000, KeyCode::Char('{')),
            (0, KeyCode::Char('a')),
            (0, KeyCode::Char('b')),
            (0, KeyCode::Tab),
        ]);
        assert_eq!(keys.last().unwrap().code, KeyCode::Char('\t'));
    }

    #[test]
    fn typed_tab_still_completes() {
        let keys = replay(&[
            (200, KeyCode::Char('s')),
            (150, KeyCode::Char('r')),
            (140, KeyCode::Char('c')),
            (300, KeyCode::Tab),
        ]);
        assert_eq!(keys.last().unwrap().code, KeyCode::Tab);
    }

    #[test]
    fn a_shortcut_breaks_the_burst() {
        let base = Instant::now();
        let mut burst = PasteBurst::new();
        for (offset, code) in [(0, 'a'), (1, 'b'), (2, 'c')] {
            burst.observe(
                press(KeyCode::Char(code)),
                base + Duration::from_millis(offset),
            );
        }
        burst.observe(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            base + Duration::from_millis(3),
        );
        let enter = burst.observe(press(KeyCode::Enter), base + Duration::from_millis(4));
        assert!(submits(&enter));
    }

    #[test]
    fn arrow_keys_break_the_burst() {
        let base = Instant::now();
        let mut burst = PasteBurst::new();
        for (offset, code) in [(0, 'a'), (1, 'b'), (2, 'c')] {
            burst.observe(
                press(KeyCode::Char(code)),
                base + Duration::from_millis(offset),
            );
        }
        burst.observe(press(KeyCode::Left), base + Duration::from_millis(3));
        let enter = burst.observe(press(KeyCode::Enter), base + Duration::from_millis(4));
        assert!(submits(&enter));
    }

    #[test]
    fn key_release_does_not_break_the_burst() {
        let base = Instant::now();
        let mut burst = PasteBurst::new();
        for (offset, code) in [(0, 'a'), (1, 'b'), (2, 'c')] {
            burst.observe(
                press(KeyCode::Char(code)),
                base + Duration::from_millis(offset),
            );
        }
        let mut release = press(KeyCode::Char('c'));
        release.kind = KeyEventKind::Release;
        burst.observe(release, base + Duration::from_millis(3));
        let enter = burst.observe(press(KeyCode::Enter), base + Duration::from_millis(4));
        assert!(!submits(&enter));
    }
}
