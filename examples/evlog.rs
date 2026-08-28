//! Logs every crossterm event to the file named by `DVZ_EVLOG`, so a ConPTY
//! injection harness can measure which written characters actually surface as
//! events — and with which `KeyEventKind` — inside a pseudo console.
//!
//!   cargo build --example evlog
//!   (a harness spawns target\debug\examples\evlog.exe under ConPTY)
//!
//! The process quits by itself after two idle seconds so a harness never has
//! to kill it.

use std::{
    fs::OpenOptions,
    io::Write,
    time::{Duration, Instant},
};

use crossterm::{
    event::{Event, poll, read},
    terminal::{disable_raw_mode, enable_raw_mode},
};

fn main() -> std::io::Result<()> {
    let path = std::env::var("DVZ_EVLOG").unwrap_or_else(|_| "evlog.txt".to_owned());
    let mut log = OpenOptions::new().create(true).append(true).open(path)?;
    enable_raw_mode()?;
    let started = Instant::now();
    let mut last = Instant::now();
    writeln!(log, "-- session start --")?;
    log.flush()?;
    loop {
        if poll(Duration::from_millis(100))? {
            let event = read()?;
            let stamp = started.elapsed().as_millis();
            if let Event::Key(key) = &event {
                writeln!(
                    log,
                    "{stamp:>6} key kind={:?} code={:?} mods={:?}",
                    key.kind, key.code, key.modifiers
                )?;
            } else {
                writeln!(log, "{stamp:>6} other {event:?}")?;
            }
            log.flush()?;
            last = Instant::now();
        } else if last.elapsed() > Duration::from_secs(2) {
            break;
        }
    }
    writeln!(log, "-- session end --")?;
    log.flush()?;
    disable_raw_mode()
}
