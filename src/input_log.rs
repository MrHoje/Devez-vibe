//! A trace of what the terminal actually delivered, kept off unless asked for.
//!
//! An IME hands its text over only when it commits, so a composer bug that only
//! shows up under a Korean keyboard cannot be read off the screen: the order and
//! the timing of the events behind it are the whole story. Setting
//! `DVZ_INPUT_LOG` to a path — or to `1` for `dvz-input-<pid>.log` in the
//! temporary directory — appends one line per event, so a session can be replayed after
//! the fact. A copy of the binary named `dvz-debug` traces on its own, so a
//! session started from a host that sets no variables still leaves a log.

use std::{
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Instant,
};

/// Appends one line, timestamped from the first record of the run. The entry is
/// built only when the log is on, so a session without it pays nothing.
pub fn record(entry: impl FnOnce() -> String) {
    let Some(sink) = sink() else {
        return;
    };
    let Ok(mut file) = sink.lock() else {
        return;
    };
    let elapsed = started().elapsed().as_secs_f64();
    let _ = writeln!(file, "{elapsed:9.3} {}", entry());
    let _ = file.flush();
}

/// True when the log is on, for callers whose entry costs more than a format.
pub fn enabled() -> bool {
    sink().is_some()
}

fn sink() -> Option<&'static Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = log_path()?;
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

fn log_path() -> Option<PathBuf> {
    let Ok(value) = env::var("DVZ_INPUT_LOG") else {
        // A copy of the binary named `dvz-debug` exists to reproduce an input
        // problem, so it traces without the variable having to be set first.
        return (running_as_debug_copy() || traces_by_default()).then(default_log_path);
    };
    match value.trim() {
        "" | "0" => None,
        "1" => Some(default_log_path()),
        path => Some(PathBuf::from(path)),
    }
}

fn default_log_path() -> PathBuf {
    // Several sessions run at once, so each one gets its own file rather than
    // interleaving with the others.
    env::temp_dir().join(format!("dvz-input-{}.log", std::process::id()))
}

/// A build made for reproducing an input problem: `DVZ_TRACE_INPUT=1` in the
/// environment at build time turns the trace on for that binary alone, so a
/// host that sets no variables still leaves a log.
fn traces_by_default() -> bool {
    matches!(option_env!("DVZ_TRACE_INPUT"), Some("1"))
}

fn running_as_debug_copy() -> bool {
    env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().eq_ignore_ascii_case("dvz-debug"))
        })
        .unwrap_or(false)
}

fn started() -> Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    *STARTED.get_or_init(Instant::now)
}
