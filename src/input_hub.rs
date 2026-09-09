//! 터미널 이벤트를 이벤트 루프와 요청 대기 루프가 함께 읽는 창구.
//!
//! 프롬프트 전송처럼 응답을 기다리는 구간은 이벤트 루프 밖에서 돌기 때문에, 그
//! 동안 누른 Esc가 응답이 올 때까지 처리되지 않았다. 스트림을 한 곳에 두고 대기
//! 루프도 같은 창구로 읽게 해, 취소는 그 자리에서 받고 나머지 입력은 이벤트 루프가
//! 원래 경로로 처리하도록 미뤄 둔다.

use std::{
    collections::VecDeque,
    io,
    sync::{Mutex as StdMutex, OnceLock},
};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::Mutex;

pub type Incoming = io::Result<Event>;

struct InputHub {
    events: Mutex<EventStream>,
    deferred: StdMutex<VecDeque<Incoming>>,
}

static HUB: OnceLock<InputHub> = OnceLock::new();

fn hub() -> &'static InputHub {
    HUB.get_or_init(|| InputHub {
        events: Mutex::new(EventStream::new()),
        deferred: StdMutex::new(VecDeque::new()),
    })
}

/// Builds the shared stream before the first reader needs it. Startup owns a
/// stream of its own, so the hub is armed only once that one is gone.
pub fn install() {
    let _ = hub();
}

pub async fn query_graphics() {
    use std::io::{IsTerminal, Write};
    if !cfg!(windows) || std::env::var_os("WT_SESSION").is_none() || !io::stdout().is_terminal() {
        return;
    }
    crate::terminal_graphics::set_size(0, 0);
    let mut events = hub().events.lock().await;
    let Some(features) = terminal_report(&mut events, "\x1b[c", "\x1b[?", 'c').await else {
        return;
    };
    if !features.iter().skip(1).any(|&feature| feature == 4) {
        return;
    }
    if let Some(pixels) = terminal_report(&mut events, "\x1b[16t", "\x1b[6;", 't').await
        && pixels.len() == 2
    {
        crate::terminal_graphics::set_size(pixels[1], pixels[0]);
    }
    let _ = io::stdout().flush();
}

async fn terminal_report(
    events: &mut EventStream,
    query: &str,
    prefix: &str,
    final_char: char,
) -> Option<Vec<u16>> {
    use std::io::Write;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    {
        let mut out = io::stdout().lock();
        out.write_all(query.as_bytes()).ok()?;
        out.flush().ok()?;
    }
    let mut pending = Vec::new();
    let mut chars = Vec::new();
    let mut char_events = Vec::new();
    while pending.len() < 512 {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.next()).await else {
            break;
        };
        if let Ok(Event::Key(key)) = &event
            && key.kind != KeyEventKind::Release
        {
            let ch = match key.code {
                KeyCode::Esc => Some('\x1b'),
                KeyCode::Char(ch) => Some(ch),
                _ => None,
            };
            if let Some(ch) = ch {
                chars.push(ch);
                char_events.push(pending.len());
            }
        }
        pending.push(event);
        if let Some((range, values)) = crate::terminal_graphics::report(&chars, prefix, final_char)
        {
            for (index, event) in pending.into_iter().enumerate() {
                if !char_events[range.clone()].contains(&index) {
                    defer(event);
                }
            }
            return Some(values);
        }
    }
    for event in pending {
        defer(event);
    }
    None
}

/// The next terminal event, taking anything a waiting request set aside first.
/// Cancel-safe: the queued event is popped without an await before the return,
/// and dropping the future only releases a lock.
pub async fn next_event() -> Option<Incoming> {
    if let Some(event) = deferred_event() {
        return Some(event);
    }
    hub().events.lock().await.next().await
}

fn deferred_event() -> Option<Incoming> {
    hub()
        .deferred
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pop_front()
}

/// Hands an event back for the event loop to process once the wait ends.
pub fn defer(event: Incoming) {
    hub()
        .deferred
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(event);
}
