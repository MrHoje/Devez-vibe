//! Screen language picked with `/language`. It switches descriptions, notices,
//! and error text; titles, buttons, labels, and shortcut hints stay English, and
//! text exchanged with a model never switches.

#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};

/// The settings.toml key, holding `korean` or `english`.
pub const CONFIG_KEY: &str = "language";

#[cfg(not(test))]
static ENGLISH: AtomicBool = AtomicBool::new(false);

// Per-thread under test, like the theme: tests run in parallel, so one test's
// `/language english` must not decide what another test reads.
#[cfg(test)]
thread_local! {
    static ENGLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(not(test))]
pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

#[cfg(not(test))]
pub fn set_english(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
}

#[cfg(test)]
pub fn english() -> bool {
    ENGLISH.with(|current| current.get())
}

#[cfg(test)]
pub fn set_english(english: bool) {
    ENGLISH.with(|current| current.set(english));
}

/// The Korean or English variant, whichever the screen language shows.
pub fn tr<T>(korean: T, english: T) -> T {
    if self::english() { english } else { korean }
}

pub const fn config_value(english: bool) -> &'static str {
    if english { "english" } else { "korean" }
}

/// Reads the saved choice. Korean is the default, so anything but `english`
/// — a missing file, a missing key, a hand-edited typo — stays Korean.
pub fn load() {
    set_english(crate::state::read_vibe_setting(CONFIG_KEY).as_deref() == Some("english"));
}
