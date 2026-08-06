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

use serde_json::{Value, json};
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
    let Some(room) = room_id() else {
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

/// Returns the DevezCode room this process belongs to, when launched by the host.
/// The same sanitized identifier is used for host state files and browser MCP calls.
pub fn room_id() -> Option<String> {
    env::var("DEVEZCODE_ROOM_ID")
        .ok()
        .map(|room| sanitize(&room))
        .filter(|room| !room.is_empty())
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

/// Mirror of the last payload handed to the host, so a turn that ends with the
/// same numbers does not touch the file (the host re-reads on every write).
static LAST_RATE_LIMITS: Mutex<Option<String>> = Mutex::new(None);

/// Publishes Claude account rate limits into the file DevezCode's usage card
/// already watches (`%APPDATA%\DevezCode\claude\ratelimit.json`), the same file
/// the `claude` CLI's statusLine hook writes.
///
/// **Claude runtime only.** The Claude Agent SDK reports fresh limits when a turn
/// ends, while the host has no hook for us and would otherwise sit on its own
/// 3-minute API poll. Codex reports its limits through `account/rateLimits/read`
/// on a different schema and a different account, and never reaches here — the
/// only caller is the `claude/account/updated` notification.
pub fn publish_claude_rate_limits(usage: Option<&Value>) {
    // Outside DevezCode there is no card to feed, and the real `claude` sessions
    // own that file.
    if room_id().is_none() {
        return;
    }
    let Some(usage) = usage else {
        return;
    };
    // A turn whose usage never arrived (SDK error, interrupted turn) must leave
    // the last good numbers alone rather than publish an empty window as 0%.
    let Some(payload) = claude_rate_limit_payload(usage) else {
        return;
    };
    let mut last = LAST_RATE_LIMITS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if last.as_deref() == Some(payload.as_str()) {
        return;
    }
    let Some(dir) = env::var_os("APPDATA")
        .map(|app_data| PathBuf::from(app_data).join("DevezCode").join("claude"))
    else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Written whole and renamed into place: the host parses this file from a
    // watcher, and a torn read would drop the window it was reading.
    // The temp name carries the pid so a concurrent `claude` hook writing the
    // same folder cannot collide with us.
    let temp = dir.join(format!("ratelimit.{}.tmp", process::id()));
    let path = dir.join("ratelimit.json");
    if fs::write(&temp, &payload).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    *last = Some(payload);
}

/// Translates the SDK's usage shape into the host's: `utilization` (0-100 float)
/// becomes `used_percentage`, and the RFC 3339 `resets_at` becomes unix seconds.
/// Returns `None` when no window carries a usable number, which keeps a partial
/// or unrelated payload from reaching the card.
fn claude_rate_limit_payload(usage: &Value) -> Option<String> {
    let mut limits = serde_json::Map::new();
    for window_name in ["five_hour", "seven_day"] {
        let Some(window) = usage.pointer(&format!("/rate_limits/{window_name}")) else {
            continue;
        };
        // Never rounded: the host compares consecutive samples to tell a real
        // early reset from the provider's occasional bogus dip.
        let Some(used) = window
            .get("utilization")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        let mut entry = json!({ "used_percentage": used.clamp(0.0, 100.0) });
        if let Some(resets_at) = window.get("resets_at").and_then(reset_epoch_seconds) {
            entry["resets_at"] = json!(resets_at);
        }
        limits.insert(window_name.to_owned(), entry);
    }
    if limits.is_empty() {
        return None;
    }
    serde_json::to_string(&json!({ "rate_limits": Value::Object(limits) })).ok()
}

/// `resets_at` is an RFC 3339 string today; a raw epoch is accepted too so a
/// server-side shape change degrades to the number instead of dropping the window.
fn reset_epoch_seconds(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|instant| instant.timestamp());
    }
    value
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| seconds as i64)
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
    fn claude_rate_limits_reach_the_host_in_its_own_shape() {
        let payload = claude_rate_limit_payload(&json!({
            "rate_limits": {
                "five_hour": { "utilization": 37.25, "resets_at": "2026-08-06T12:00:00Z" },
                "seven_day": { "utilization": 12.0, "resets_at": "2026-08-10T00:00:00Z" },
            }
        }))
        .expect("both windows are usable");
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        // Not rounded: the host's drop guard compares consecutive samples.
        assert_eq!(
            parsed["rate_limits"]["five_hour"]["used_percentage"],
            json!(37.25)
        );
        assert_eq!(
            parsed["rate_limits"]["five_hour"]["resets_at"],
            json!(1_786_017_600_i64)
        );
        assert_eq!(
            parsed["rate_limits"]["seven_day"]["used_percentage"],
            json!(12.0)
        );
    }

    #[test]
    fn windows_without_a_number_are_left_out_rather_than_published_as_zero() {
        let payload = claude_rate_limit_payload(&json!({
            "rate_limits": {
                "five_hour": { "utilization": 5.0 },
                "seven_day": { "resets_at": "2026-08-10T00:00:00Z" },
            }
        }))
        .expect("the five hour window is usable");
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed["rate_limits"]["five_hour"]["used_percentage"],
            json!(5.0)
        );
        // No reset is better than a made-up one, and an empty window is dropped.
        assert!(
            parsed["rate_limits"]["five_hour"]
                .get("resets_at")
                .is_none()
        );
        assert!(parsed["rate_limits"].get("seven_day").is_none());
    }

    #[test]
    fn a_payload_without_usable_windows_is_not_published() {
        assert!(claude_rate_limit_payload(&json!({})).is_none());
        assert!(claude_rate_limit_payload(&json!({ "rate_limits": {} })).is_none());
        // Codex's own shape (`account/rateLimits/read`) must never be mistaken
        // for Claude usage if it ever reached this path.
        assert!(
            claude_rate_limit_payload(&json!({
                "rate_limits": { "primary": { "used_percent": 40 } }
            }))
            .is_none()
        );
    }

    #[test]
    fn a_numeric_reset_is_accepted_as_epoch_seconds() {
        assert_eq!(
            reset_epoch_seconds(&json!("2026-08-06T12:00:00Z")),
            Some(1_786_017_600)
        );
        assert_eq!(
            reset_epoch_seconds(&json!(1_786_017_600_i64)),
            Some(1_786_017_600)
        );
        assert_eq!(reset_epoch_seconds(&json!("not a time")), None);
        assert_eq!(reset_epoch_seconds(&json!(0)), None);
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
