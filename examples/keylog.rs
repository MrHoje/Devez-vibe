//! Measures the gap between key events so the paste threshold can be chosen
//! from data instead of guessed.
//!
//! Bracketed paste is unreachable here — the markers only arrive under VT
//! input mode, which costs crossterm's whole key decoder — so telling a paste
//! from typing comes down to how fast the keys arrive. This prints that gap
//! for every key, in the order they are handled, which is the same vantage
//! point the application has.
//!
//!   cargo run --example keylog
//!
//! Two samples are needed, and they have to be kept apart:
//!   1. type a short sentence and press Enter
//!   2. paste two or three short lines
//!
//! Esc twice quits. This probe never changes the console mode, so quitting it
//! any way at all leaves the terminal as it found it.

use std::{
    io::{Write, stdout},
    time::Instant,
};

use crossterm::{
    event::{Event, KeyCode, KeyEventKind, KeyModifiers, read},
    terminal::{disable_raw_mode, enable_raw_mode},
};

fn main() -> std::io::Result<()> {
    println!("gap(ms)  key");
    println!("type a sentence + Enter, then paste 2-3 lines; Esc twice quits");
    stdout().flush()?;

    enable_raw_mode()?;
    let mut previous: Option<Instant> = None;
    let mut escapes = 0;
    loop {
        let event = read()?;
        let now = Instant::now();
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let gap = previous.map(|last| now.duration_since(last).as_millis());
        previous = Some(now);

        let shown = match key.code {
            KeyCode::Char(ch) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                ch.to_string()
            }
            KeyCode::Enter => "ENTER".to_owned(),
            KeyCode::Tab => "TAB".to_owned(),
            other => format!("{other:?} {:?}", key.modifiers),
        };
        match gap {
            Some(gap) => println!("{gap:>7}  {shown}\r"),
            None => println!("      -  {shown}\r"),
        }
        let _ = stdout().flush();

        if key.code == KeyCode::Esc {
            escapes += 1;
            if escapes == 2 {
                break;
            }
        } else {
            escapes = 0;
        }
    }

    disable_raw_mode()
}
