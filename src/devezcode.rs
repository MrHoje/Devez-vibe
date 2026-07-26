//! Status hand-off to DevezCode, the WPF host that runs this CLI inside a tab.
//!
//! DevezCode launches one CLI per room and passes the room's id down as
//! `DEVEZCODE_ROOM_ID`. Everything it shows around the terminal — the busy
//! spinner, the ❗ waiting badge, the header's last prompt, and which session to
//! resume next time — is read from
//! `%APPDATA%\DevezCode\devezcli\{sessions,busy,waiting,lastmsg}\<room>.txt`, so
//! this module keeps those four files in step with the session.
//!
//! Outside DevezCode the variable is absent and every function here is a no-op:
//! a plain `dvz` in a plain terminal writes nothing.

use std::{
    env, fs,
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

/// The vocabulary DevezCode's watchers expect. `busy` is read as "running or
/// not", `waiting` as "waiting or not"; the idle words only have to differ.
const BUSY: &str = "running";
const IDLE: &str = "idle";
const WAITING: &str = "waiting";
const READY: &str = "ready";

/// Longest header line DevezCode renders. Cut by characters, not bytes, so a
/// Korean prompt is never split mid-glyph.
const SUMMARY_CHARS: usize = 200;

static REPORTER: Mutex<Option<Reporter>> = Mutex::new(None);

struct Reporter {
    base: PathBuf,
    room: String,
    /// Mirrors of what is already on disk. A turn ticks several times a second
    /// and almost none of those ticks change anything, so the files are only
    /// rewritten when a value actually moves.
    session: String,
    busy: bool,
    waiting: bool,
}

/// Binds this process to its DevezCode room, if it has one. Call once at
/// startup: the room is fixed for the life of the process.
pub fn init() {
    let Some(room) = env::var("DEVEZCODE_ROOM_ID")
        .ok()
        .map(|room| sanitize(&room))
        .filter(|room| !room.is_empty())
    else {
        return;
    };
    let Some(base) = env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("DevezCode")
            .join("devezcli")
    }) else {
        return;
    };

    let reporter = Reporter {
        base,
        room,
        session: String::new(),
        busy: false,
        waiting: false,
    };
    // A previous run that was killed mid-turn leaves `running` behind, and the
    // spinner would spin from the first frame of this one.
    reporter.write("busy", IDLE);
    reporter.write("waiting", READY);
    if let Ok(mut slot) = REPORTER.lock() {
        *slot = Some(reporter);
    }
}

/// Publishes the session state DevezCode paints around the terminal. Called
/// from every frame; cheap when nothing changed.
pub fn sync(thread_id: &str, busy: bool, waiting: bool) {
    with(|reporter| {
        if !thread_id.is_empty() && reporter.session != thread_id {
            reporter.session = thread_id.to_owned();
            reporter.write("sessions", thread_id);
        }
        if reporter.busy != busy {
            reporter.busy = busy;
            reporter.write("busy", if busy { BUSY } else { IDLE });
        }
        if reporter.waiting != waiting {
            reporter.waiting = waiting;
            reporter.write("waiting", if waiting { WAITING } else { READY });
        }
    });
}

/// Records the prompt that was just sent, which DevezCode shows in the session
/// header. Blank prompts are ignored so the header keeps the last real one.
pub fn note_prompt(text: &str) {
    let summary = summarize(text);
    if summary.is_empty() {
        return;
    }
    with(|reporter| reporter.write("lastmsg", &summary));
}

/// Clears the transient state on the way out, so a closed session does not sit
/// there spinning in the host.
pub fn finish() {
    with(|reporter| {
        reporter.busy = false;
        reporter.waiting = false;
        reporter.write("busy", IDLE);
        reporter.write("waiting", READY);
    });
}

fn with(action: impl FnOnce(&mut Reporter)) {
    // A panic elsewhere must not silence the status files for the rest of the
    // run; the data behind the lock is four plain fields.
    let mut slot = REPORTER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(reporter) = slot.as_mut() {
        action(reporter);
    }
}

impl Reporter {
    fn write(&self, kind: &str, value: &str) {
        let dir = self.base.join(kind);
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        // Written whole and renamed into place: a reader that catches a
        // truncated `busy` file reads it as idle and drops the spinner
        // mid-turn.
        let temp = dir.join(format!("{}.tmp", self.room));
        let path = dir.join(format!("{}.txt", self.room));
        if fs::write(&temp, value).is_err() {
            let _ = fs::remove_file(&temp);
            return;
        }
        if fs::rename(&temp, &path).is_err() {
            let _ = fs::remove_file(&temp);
        }
    }
}

/// Room ids reach the file system as names, and DevezCode strips the same set
/// on its side when it looks them up.
fn sanitize(room: &str) -> String {
    room.chars()
        .filter(|character| character.is_alphanumeric() || *character == '_' || *character == '-')
        .collect()
}

/// First non-blank line of a prompt, capped for the one-line header.
fn summarize(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(SUMMARY_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_ids_lose_path_characters() {
        assert_eq!(sanitize("room-1_a"), "room-1_a");
        assert_eq!(sanitize("../etc/room 2"), "etcroom2");
        assert_eq!(sanitize("///"), "");
    }

    #[test]
    fn summary_takes_the_first_real_line() {
        assert_eq!(summarize("\n\n  빌드 고쳐줘  \n다음 줄"), "빌드 고쳐줘");
        assert_eq!(summarize("   "), "");
    }

    #[test]
    fn summary_cuts_on_character_boundaries() {
        let long = "가".repeat(SUMMARY_CHARS + 50);
        assert_eq!(summarize(&long).chars().count(), SUMMARY_CHARS);
    }
}
