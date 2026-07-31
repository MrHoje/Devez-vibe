//! Status hand-off to DevezCode, the WPF host that runs this CLI inside a tab.
//!
//! DevezCode launches one CLI per room and passes the room's id down as
//! `DEVEZCODE_ROOM_ID`. Everything it shows around the terminal — the busy
//! spinner, the ❗ waiting badge, the header's last prompt, and which session to
//! resume next time — is read from
//! `%APPDATA%\DevezCode\devezvibe\{sessions,busy,waiting,lastmsg}\<room>.txt`, so
//! this module keeps those four files in step with the session.
//!
//! Outside DevezCode the variable is absent and every function here is a no-op:
//! a plain `dvz` in a plain terminal writes nothing.

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    process,
    sync::{Mutex, PoisonError},
};

/// The vocabulary DevezCode's watchers expect. `busy` is read as "running or
/// not", `waiting` as "waiting or not"; the idle words only have to differ.
const BUSY: &str = "running";
const LOADING: &str = "loading";
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
    owner_token: String,
    /// Mirrors of what is already on disk. A turn ticks several times a second
    /// and almost none of those ticks change anything, so the files are only
    /// rewritten when a value actually moves.
    session: String,
    activity: Activity,
    waiting: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Idle,
    Running,
    Loading,
}

impl Activity {
    fn from_host(busy: bool, loading: bool) -> Self {
        if busy {
            Self::Running
        } else if loading {
            Self::Loading
        } else {
            Self::Idle
        }
    }

    fn status(self) -> &'static str {
        match self {
            Self::Idle => IDLE,
            Self::Running => BUSY,
            Self::Loading => LOADING,
        }
    }
}

/// Binds this process to its DevezCode room, if it has one. Call once at
/// startup: the room is fixed for the life of the process.
pub fn init() {
    let Some(folder) = tracking_agent_folder(env::var("DEVEZCODE_TRACKING_AGENT").ok().as_deref())
    else {
        return;
    };
    let Some(room) = env::var("DEVEZCODE_ROOM_ID")
        .ok()
        .map(|room| sanitize(&room))
        .filter(|room| !room.is_empty())
    else {
        return;
    };
    let Some(base) = env::var_os("APPDATA")
        .map(|app_data| PathBuf::from(app_data).join("DevezCode").join(folder))
    else {
        return;
    };
    let owner_token = process::id().to_string();
    if !try_claim_owner(&base, &room, &owner_token) {
        return;
    }

    let reporter = Reporter {
        base,
        room,
        owner_token,
        session: String::new(),
        activity: Activity::Idle,
        waiting: false,
    };
    // A previous run that was killed mid-turn leaves `running` behind, and the
    // spinner would spin from the first frame of this one.
    reporter.write("busy", IDLE);
    reporter.write("waiting", READY);
    let mut slot = REPORTER.lock().unwrap_or_else(PoisonError::into_inner);
    *slot = Some(reporter);
}

/// Publishes the session state DevezCode paints around the terminal. Called
/// from every frame; cheap when nothing changed.
pub fn sync(thread_id: &str, busy: bool, loading: bool, waiting: bool) {
    with(|reporter| {
        if !thread_id.is_empty() && reporter.session != thread_id {
            reporter.session = thread_id.to_owned();
            reporter.write("sessions", thread_id);
        }
        let activity = Activity::from_host(busy, loading);
        if reporter.activity != activity {
            reporter.activity = activity;
            reporter.write("busy", activity.status());
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
    let mut slot = REPORTER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(mut reporter) = slot.take() {
        reporter.activity = Activity::Idle;
        reporter.waiting = false;
        reporter.write("busy", IDLE);
        reporter.write("waiting", READY);
        release_owner(&reporter.base, &reporter.room, &reporter.owner_token);
    }
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
        if !owns_room(&self.base, &self.room, &self.owner_token) {
            return;
        }
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
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn host_activity_keeps_resume_loading_distinct_from_a_turn() {
        assert_eq!(Activity::from_host(false, false).status(), "idle");
        assert_eq!(Activity::from_host(true, false).status(), "running");
        assert_eq!(Activity::from_host(false, true).status(), "loading");
        assert_eq!(Activity::from_host(true, true).status(), "running");
    }

    #[test]
    fn tracking_agent_picks_the_host_state_folder() {
        assert_eq!(tracking_agent_folder(Some("devezvibe")), Some("devezvibe"));
        assert_eq!(tracking_agent_folder(Some("DevezVibe")), Some("devezvibe"));
        // Hosts from before the rename keep their own folder name.
        assert_eq!(tracking_agent_folder(Some("devezcli")), Some("devezcli"));
        assert_eq!(tracking_agent_folder(Some("DevezCLI")), Some("devezcli"));
        assert_eq!(tracking_agent_folder(Some("claude")), None);
        assert_eq!(tracking_agent_folder(None), None);
    }

    #[test]
    fn first_process_owns_the_room_until_it_releases() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = env::temp_dir().join(format!("devezcli-owner-{}-{nonce}", std::process::id()));
        let room = "room-test";

        assert!(try_claim_owner(&base, room, "root"));
        assert!(!try_claim_owner(&base, room, "nested"));
        assert!(owns_room(&base, room, "root"));
        assert!(!owns_room(&base, room, "nested"));

        release_owner(&base, room, "nested");
        assert!(owns_room(&base, room, "root"));
        release_owner(&base, room, "root");
        assert!(try_claim_owner(&base, room, "next-root"));

        let _ = fs::remove_dir_all(base);
    }
}

/// The agent ids DevezCode may announce us under. `devezvibe` is the current
/// one; `devezcli` is what hosts released before the rename still send, and it
/// also names the state folder they watch — so the folder is derived from the
/// value instead of hard-coded, and either host version lines up.
const TRACKING_AGENTS: [&str; 2] = ["devezvibe", "devezcli"];

fn tracking_agent_folder(agent: Option<&str>) -> Option<&'static str> {
    let agent = agent?;
    TRACKING_AGENTS
        .into_iter()
        .find(|known| agent.eq_ignore_ascii_case(known))
}

fn owner_path(base: &std::path::Path, room: &str) -> PathBuf {
    base.join("owners").join(format!("{room}.txt"))
}

fn try_claim_owner(base: &std::path::Path, room: &str, token: &str) -> bool {
    let path = owner_path(base, room);
    let Some(dir) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(&path) else {
        return false;
    };
    if file.write_all(token.as_bytes()).is_ok() {
        true
    } else {
        drop(file);
        let _ = fs::remove_file(path);
        false
    }
}

fn owns_room(base: &std::path::Path, room: &str, token: &str) -> bool {
    fs::read_to_string(owner_path(base, room)).is_ok_and(|owner| owner.trim() == token)
}

fn release_owner(base: &std::path::Path, room: &str, token: &str) {
    if owns_room(base, room, token) {
        let _ = fs::remove_file(owner_path(base, room));
    }
}
