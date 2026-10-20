//! Host-side Computer Use cancellation. The native helper owns the overlay and
//! input injection; a tool error or its interrupt marker is NOT a turn interrupt.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    app_server::{AppServerClient, ServerEvent},
    language::tr,
};

const PHYSICAL_ESCAPE: &str = "Computer Use was stopped by the user with the physical Escape key.";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Turn {
    thread: String,
    id: String,
}

impl Turn {
    fn from_params(params: &Value) -> Option<Self> {
        let thread = params.get("threadId")?.as_str()?;
        let id = params
            .pointer("/turn/id")
            .or_else(|| params.get("turnId"))?
            .as_str()?;
        if thread.is_empty() || id.is_empty() {
            return None;
        }
        Some(Self {
            thread: thread.to_owned(),
            id: id.to_owned(),
        })
    }

    fn params(&self) -> Value {
        json!({ "threadId": self.thread, "turnId": self.id })
    }
}

#[derive(Default)]
struct ActiveTurn {
    computer: bool,
    armed_generation: u64,
    stop_requested: bool,
    stop_version: u64,
    interrupt_sent: bool,
    interrupt_in_flight: bool,
    marker_seen: bool,
    session: Option<String>,
    cwd: Option<String>,
}

#[derive(Default, Clone)]
struct ThreadInfo {
    session: Option<String>,
    cwd: Option<String>,
}

struct Ended {
    turn: Turn,
    cwd: Option<String>,
    computer: bool,
    abnormal: bool,
}

#[derive(Default)]
struct State {
    turns: BTreeMap<Turn, ActiveTurn>,
    generation: u64,
    threads: BTreeMap<String, ThreadInfo>,
    stopped: bool,
    pending_cwd: BTreeMap<String, String>,
}

impl State {
    fn register_thread(&mut self, thread: &Value, fallback_cwd: Option<&str>) {
        let Some(id) = thread.get("id").and_then(Value::as_str) else {
            return;
        };
        let session = thread
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(
                || match thread.get("parentThreadId").and_then(Value::as_str) {
                    Some(parent) => self
                        .threads
                        .get(parent)
                        .and_then(|info| info.session.clone()),
                    None => Some(id.to_owned()),
                },
            );
        let cwd = thread
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .or(fallback_cwd)
            .map(str::to_owned);
        self.threads.insert(
            id.to_owned(),
            ThreadInfo {
                session: session.clone(),
                cwd: cwd.clone(),
            },
        );
        for (turn, active) in &mut self.turns {
            if turn.thread == id {
                active.session = session.clone();
                if active.cwd.is_none() {
                    active.cwd = cwd.clone();
                }
            }
        }
    }

    fn observe(&mut self, method: &str, params: &Value) -> Vec<Ended> {
        let previous = self.targets();
        let ended = self.observe_inner(method, params);
        self.rearm(previous);
        ended
    }

    fn rearm(&mut self, previous: Vec<Turn>) {
        if previous != self.targets() {
            self.generation = self.generation.wrapping_add(1).max(1);
        }
        for active in self
            .turns
            .values_mut()
            .filter(|active| active.computer && active.armed_generation == 0)
        {
            active.armed_generation = self.generation;
        }
    }

    fn observe_inner(&mut self, method: &str, params: &Value) -> Vec<Ended> {
        if method == "thread/started" {
            self.register_thread(&params["thread"], None);
            return Vec::new();
        }
        if method == "thread/closed" {
            let Some(thread) = params.get("threadId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let turns: Vec<_> = self
                .turns
                .keys()
                .filter(|turn| turn.thread == thread)
                .cloned()
                .collect();
            self.threads.remove(thread);
            return turns
                .into_iter()
                .filter_map(|turn| self.end(turn, true))
                .collect();
        }
        let Some(turn) = Turn::from_params(params) else {
            return Vec::new();
        };
        match method {
            "turn/started" => {
                // A delayed terminal event for an old turn must never disarm its successor.
                let old: Vec<_> = self
                    .turns
                    .keys()
                    .filter(|key| key.thread == turn.thread && **key != turn)
                    .cloned()
                    .collect();
                // Replacement isn't itself proof of an abnormal terminal status.
                let ended = old
                    .into_iter()
                    .filter_map(|turn| self.end(turn, false))
                    .collect();
                let info = self
                    .threads
                    .get(&turn.thread)
                    .cloned()
                    .unwrap_or_else(|| ThreadInfo {
                        session: Some(turn.thread.clone()),
                        cwd: None,
                    });
                let cwd = self.pending_cwd.get(&turn.thread).cloned().or(info.cwd);
                self.turns.entry(turn).or_insert_with(|| ActiveTurn {
                    session: info.session,
                    cwd,
                    ..Default::default()
                });
                return ended;
            }
            "item/started" | "item/completed" => {
                let item = &params["item"];
                let computer = is_computer_item(item);
                let stopped = is_computer_stop(item);
                if let Some(active) = self.turns.get_mut(&turn) {
                    active.computer |= computer || stopped;
                    if stopped && !active.stop_requested {
                        active.stop_version = active.stop_version.wrapping_add(1);
                        active.stop_requested = true;
                    }
                }
            }
            "turn/completed" | "turn/failed" | "turn/aborted" => {
                let abnormal = method != "turn/completed"
                    || matches!(
                        params.pointer("/turn/status").and_then(Value::as_str),
                        Some("interrupted" | "failed")
                    );
                return self.end(turn, abnormal).into_iter().collect();
            }
            _ => {}
        }
        Vec::new()
    }

    fn end(&mut self, turn: Turn, abnormal: bool) -> Option<Ended> {
        self.turns.remove(&turn).map(|active| Ended {
            turn,
            cwd: active.cwd,
            computer: active.computer,
            abnormal,
        })
    }

    fn targets(&self) -> Vec<Turn> {
        self.turns
            .iter()
            .filter(|(_, active)| active.computer)
            .map(|(turn, _)| turn.clone())
            .collect()
    }

    fn escape(&mut self, generation: u64) {
        // Events carry the arm generation from key-down, not whichever turn is
        // current when the async receiver happens to wake up.
        if generation == 0 || generation > self.generation {
            return;
        }
        for active in self
            .turns
            .values_mut()
            .filter(|active| active.computer && active.armed_generation <= generation)
        {
            active.stop_requested = true;
            active.stop_version = active.stop_version.wrapping_add(1);
            active.interrupt_sent = false; // a fresh press can retry a failed stop
        }
    }

    fn mark_interrupts(&mut self, home: Option<&Path>) {
        let previous = self.targets();
        for (turn, active) in &mut self.turns {
            if !active.marker_seen
                && home.is_some_and(|home| {
                    active.session.as_ref().is_some_and(|session| {
                        interrupt_marker(
                            home,
                            &Turn {
                                thread: session.clone(),
                                id: turn.id.clone(),
                            },
                        )
                        .is_some_and(|path| path.is_file())
                    })
                })
            {
                active.marker_seen = true;
                active.computer = true;
                active.stop_version = active.stop_version.wrapping_add(1);
                active.stop_requested = true;
            }
        }
        self.rearm(previous);
    }

    fn take_interrupts(&mut self) -> Vec<(Turn, u64)> {
        self.turns
            .iter_mut()
            .filter_map(|(turn, active)| {
                if !active.stop_requested || active.interrupt_sent || active.interrupt_in_flight {
                    return None;
                }
                active.interrupt_sent = true;
                active.interrupt_in_flight = true;
                Some((turn.clone(), active.stop_version))
            })
            .collect()
    }
}

fn is_computer_item(item: &Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("mcpToolCall") {
        return false;
    }
    match item.get("server").and_then(Value::as_str) {
        Some("cua_repl") => matches!(
            item.get("tool").and_then(Value::as_str),
            Some("js" | "js_reset")
        ),
        Some("node_repl") if item.get("tool").and_then(Value::as_str) == Some("js") => {
            // Legacy Desktop installs expose CU through the generic JS server.
            // Never arm for unrelated JS, shell output, or arbitrary tool prose.
            item.pointer("/arguments/code")
                .and_then(Value::as_str)
                .is_some_and(|code| {
                    ["@oai/sky", "@oai/cua", "sky.", "cua."]
                        .iter()
                        .any(|name| code.contains(name))
                })
        }
        _ => false,
    }
}

fn is_computer_stop(item: &Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("mcpToolCall")
        || !matches!(
            item.get("server").and_then(Value::as_str),
            Some("node_repl" | "cua_repl")
        )
    {
        return false;
    }
    let is_stop = |text: &str| {
        text.trim_start()
            .strip_prefix("Error: ")
            .unwrap_or(text.trim_start())
            .starts_with(PHYSICAL_ESCAPE)
    };
    item.pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(is_stop)
        || item
            .pointer("/result/content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(is_stop)
                })
            })
}

/// Compatibility signal used by @oai/sky's direct Windows helper transport.
/// Native-pipe installs need not create it; neither transport depends on it here.
fn interrupt_marker(home: &Path, turn: &Turn) -> Option<PathBuf> {
    let component = |value: &str| {
        let sanitized: String = value
            .encode_utf16()
            .map(|unit| {
                if unit <= 127
                    && ((unit as u8).is_ascii_alphanumeric() || b"._-".contains(&(unit as u8)))
                {
                    unit as u8 as char
                } else {
                    '_'
                }
            })
            .collect();
        (!matches!(sanitized.as_str(), "" | "." | "..")).then_some(sanitized)
    };
    Some(
        home.join("cache/computer-use/interrupts")
            .join(component(&turn.thread)?)
            .join(component(&turn.id)?),
    )
}

#[derive(Clone)]
pub struct Observer {
    state: Arc<Mutex<State>>,
    enabled: Arc<AtomicBool>,
    recovery: Arc<Mutex<Option<JoinHandle<()>>>>,
    events: mpsc::UnboundedSender<ServerEvent>,
    arm: crate::computer_use_input::EscapeArmHandle,
    hook_error: Arc<Mutex<Option<String>>>,
    config_path: Option<PathBuf>,
    cleanup: Arc<Mutex<Vec<JoinHandle<()>>>>,
    probe_gate: Arc<Mutex<bool>>,
}

impl Observer {
    pub fn observe(&self, message: &Value) {
        if message.get("id").is_some() {
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let params = &message["params"];
        let mut state = self.state.lock().expect("computer-use state");
        if state.stopped {
            return;
        }
        let ended = state.observe(method, params);
        let next = state.targets();
        *self.probe_gate.lock().expect("cursor probe gate") = next.is_empty();
        self.arm
            .arm(if next.is_empty() { 0 } else { state.generation });
        if !next.is_empty() {
            self.enabled.store(true, Ordering::Release);
            if let Some(error) = self.hook_error.lock().expect("hook status").take() {
                let _ = self.events.send(ServerEvent::ProtocolWarning(tr(
                    format!("Computer Use 전역 Esc 감지를 시작하지 못했습니다. 터미널에서 Esc로 중단하세요: {error}"),
                    format!("Couldn't start Computer Use's global Escape listener. Use Escape in the terminal to stop: {error}"),
                )));
            }
            self.cancel_recovery();
        }
        drop(state);
        let needs_recovery = ended.iter().any(|end| end.computer)
            || matches!(method, "turn/completed" | "turn/failed" | "turn/aborted");
        self.cleanup_ended(ended);
        if needs_recovery {
            self.recover();
        }
    }

    pub fn register_thread(&self, response: &Value, cwd: Option<&str>) {
        self.state
            .lock()
            .expect("computer-use state")
            .register_thread(
                &response["thread"],
                response.get("cwd").and_then(Value::as_str).or(cwd),
            );
    }

    pub fn begin_turn_request(&self, thread: Option<&str>, cwd: Option<&str>) {
        if let (Some(thread), Some(cwd)) = (thread, cwd) {
            let mut state = self.state.lock().expect("computer-use state");
            if !state.stopped {
                state.pending_cwd.insert(thread.to_owned(), cwd.to_owned());
            }
        }
    }

    pub fn end_turn_request(&self, thread: Option<&str>, cwd: Option<&str>) {
        if let (Some(thread), Some(cwd)) = (thread, cwd) {
            let mut state = self.state.lock().expect("computer-use state");
            if state
                .pending_cwd
                .get(thread)
                .is_some_and(|pending| pending == cwd)
            {
                state.pending_cwd.remove(thread);
            }
        }
    }

    pub fn register_turn_response(
        &self,
        thread: Option<&str>,
        response: &Value,
        cwd: Option<&str>,
    ) {
        let (Some(thread), Some(cwd)) = (thread, cwd) else {
            return;
        };
        let mut state = self.state.lock().expect("computer-use state");
        if state.stopped {
            return;
        }
        state.threads.entry(thread.to_owned()).or_default().cwd = Some(cwd.to_owned());
        if let Some(id) = response.pointer("/turn/id").and_then(Value::as_str) {
            let turn = Turn {
                thread: thread.to_owned(),
                id: id.to_owned(),
            };
            if let Some(active) = state.turns.get_mut(&turn) {
                active.cwd = Some(cwd.to_owned());
            }
        }
    }

    fn cleanup_ended(&self, ended: Vec<Ended>) {
        let Some(config) = &self.config_path else {
            return;
        };
        let mut cleanup = self.cleanup.lock().expect("computer-use cleanup");
        cleanup.retain(|task| !task.is_finished());
        for end in ended.into_iter().filter(|end| end.computer && end.abnormal) {
            let Some(cwd) = end.cwd else {
                continue;
            };
            let config = config.clone();
            let events = self.events.clone();
            let state = self.state.clone();
            cleanup.push(tokio::spawn(async move {
                if let Err(error) = crate::computer_use_notify::notify(
                    &config,
                    &end.turn.thread,
                    &end.turn.id,
                    &cwd,
                    |command| {
                        // Serialize the launch check with item/started. If another CU
                        // turn took over, its normal terminal cleanup owns the helper.
                        let state = state.lock().expect("computer-use state");
                        if !state.targets().is_empty() {
                            return Ok(None);
                        }
                        command.spawn().map(Some)
                    },
                )
                .await
                {
                    let _ = events.send(ServerEvent::ProtocolWarning(tr(
                        format!("Computer Use 종료 알림 실패: {error}"),
                        format!(
                            "Couldn't notify the Computer Use helper that its turn ended: {error}"
                        ),
                    )));
                }
            }));
        }
    }

    pub fn recover(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        // Do not run an old turn's delayed recovery over a still-live CU turn.
        let state = self.state.lock().expect("computer-use state");
        if state.stopped || !state.targets().is_empty() {
            return;
        }
        let mut recovery = self.recovery.lock().expect("cursor recovery task");
        if recovery.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let events = self.events.clone();
        let gate = self.probe_gate.clone();
        *recovery = Some(tokio::spawn(async move {
            if let Err(error) = crate::cursor_recovery::restore_after_turn_guarded(gate).await {
                let _ = events.send(ServerEvent::ProtocolWarning(tr(
                    format!("Windows 마우스 커서 자동 복원 실패: {error}"),
                    format!("Couldn't restore the Windows mouse cursor: {error}"),
                )));
            }
        }));
    }

    fn cancel_recovery(&self) {
        if let Some(task) = self.recovery.lock().expect("cursor recovery task").take() {
            task.abort();
        }
    }

    pub fn closed(&self) {
        let ended = {
            let mut state = self.state.lock().expect("computer-use state");
            let turns: Vec<_> = state.turns.keys().cloned().collect();
            let ended = turns
                .into_iter()
                .filter_map(|turn| state.end(turn, true))
                .collect();
            self.arm.arm(0);
            *self.probe_gate.lock().expect("cursor probe gate") = !state.stopped;
            ended
        };
        self.cleanup_ended(ended);
        self.recover();
    }
}

pub struct Monitor {
    observer: Observer,
    task: JoinHandle<()>,
}

impl Monitor {
    pub fn start(
        client: AppServerClient,
        events: mpsc::UnboundedSender<ServerEvent>,
        home: Option<PathBuf>,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        // The listener starts disarmed; arming in observe() is synchronous with
        // item/started, not delayed until an async polling tick.
        let (hook, input, arm, hook_error) = if cfg!(all(windows, not(test))) {
            match crate::computer_use_input::PhysicalEscape::start() {
                Ok((hook, input)) => {
                    let arm = hook.arm_handle();
                    (Some(hook), Some(input), arm, None)
                }
                Err(error) => (None, None, Default::default(), Some(error.to_string())),
            }
        } else {
            (None, None, Default::default(), None)
        };
        Self::start_inner(client, events, home, enabled, hook, input, arm, hook_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_inner(
        client: AppServerClient,
        events: mpsc::UnboundedSender<ServerEvent>,
        home: Option<PathBuf>,
        enabled: Arc<AtomicBool>,
        hook: Option<crate::computer_use_input::PhysicalEscape>,
        mut input: Option<mpsc::UnboundedReceiver<u64>>,
        arm: crate::computer_use_input::EscapeArmHandle,
        hook_error: Option<String>,
    ) -> Self {
        let observer = Observer {
            state: Arc::new(Mutex::new(State::default())),
            enabled,
            recovery: Arc::new(Mutex::new(None)),
            events,
            arm,
            hook_error: Arc::new(Mutex::new(hook_error)),
            config_path: home.as_ref().map(|home| home.join("config.toml")),
            cleanup: Arc::new(Mutex::new(Vec::new())),
            probe_gate: Arc::new(Mutex::new(true)),
        };
        let shared = observer.clone();
        let task = tokio::spawn(async move {
            let _hook = hook; // the task owns installation and deterministic unhooking
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut requests = tokio::task::JoinSet::new();
            loop {
                let escape = tokio::select! {
                    _ = interval.tick() => None,
                    event = async {
                        match input.as_mut() {
                            Some(input) => input.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if event.is_none() { input = None; }
                        event
                    }
                };
                let stops = {
                    let mut state = shared.state.lock().expect("computer-use state");
                    if state.stopped {
                        break;
                    }
                    state.mark_interrupts(home.as_deref());
                    if let Some(generation) = escape {
                        state.escape(generation);
                    }
                    let armed = !state.targets().is_empty();
                    *shared.probe_gate.lock().expect("cursor probe gate") = !armed;
                    shared.arm.arm(if armed { state.generation } else { 0 });
                    if armed {
                        shared.enabled.store(true, Ordering::Release);
                        shared.cancel_recovery();
                    }
                    state.take_interrupts()
                };
                for (turn, version) in stops {
                    let client = client.clone();
                    let shared = shared.clone();
                    requests.spawn(async move {
                        // The RPC ack is not completion. Keep the CU state armed
                        // until the precise turn's terminal notification arrives.
                        let result = client.request("turn/interrupt", turn.params()).await;
                        if let Some(active) = shared.state.lock().expect("computer-use state").turns.get_mut(&turn) {
                            active.interrupt_in_flight = false;
                            let newer_press = active.stop_version != version;
                            active.interrupt_sent = result.is_ok() && !newer_press;
                            if result.is_err() && !newer_press { active.stop_requested = false; }
                        }
                        if let Err(error) = result {
                            let _ = shared.events.send(ServerEvent::ProtocolWarning(tr(
                                format!("Computer Use 중단 요청 실패. Esc를 다시 눌러 재시도하세요: {error}"),
                                format!("Couldn't interrupt Computer Use. Press Escape again to retry: {error}"),
                            )));
                        }
                    });
                }
                while requests.try_join_next().is_some() {}
            }
        });
        Self { observer, task }
    }

    #[cfg(test)]
    pub(crate) fn start_with_test_input(
        client: AppServerClient,
        events: mpsc::UnboundedSender<ServerEvent>,
        home: Option<PathBuf>,
        input: mpsc::UnboundedReceiver<u64>,
    ) -> Self {
        Self::start_inner(
            client,
            events,
            home,
            Arc::new(AtomicBool::new(false)),
            None,
            Some(input),
            Default::default(),
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.observer
            .state
            .lock()
            .expect("computer-use state")
            .generation
    }

    pub fn observer(&self) -> Observer {
        self.observer.clone()
    }

    pub async fn finish_cleanup(&self) {
        self.observer.closed();
        let cleanup =
            std::mem::take(&mut *self.observer.cleanup.lock().expect("computer-use cleanup"));
        for task in cleanup {
            let _ = task.await;
        }
        self.observer.cancel_recovery();
    }

    pub fn stop(&self) {
        let mut state = self.observer.state.lock().expect("computer-use state");
        state.stopped = true;
        self.observer.arm.arm(0);
        *self.observer.probe_gate.lock().expect("cursor probe gate") = false;
        self.task.abort();
        self.observer.cancel_recovery();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
        for task in self
            .observer
            .cleanup
            .lock()
            .expect("computer-use cleanup")
            .drain(..)
        {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(state: &mut State) -> Vec<Turn> {
        state
            .take_interrupts()
            .into_iter()
            .map(|(turn, _)| turn)
            .collect()
    }
    fn turn(thread: &str, id: &str) -> Turn {
        Turn {
            thread: thread.into(),
            id: id.into(),
        }
    }
    fn started(thread: &str, id: &str) -> Value {
        json!({"threadId":thread,"turn":{"id":id}})
    }
    fn item(thread: &str, id: &str, server: &str, code: &str) -> Value {
        json!({"threadId":thread,"turnId":id,"item":{"type":"mcpToolCall","server":server,"tool":"js","arguments":{"code":code}}})
    }

    #[test]
    fn only_live_computer_calls_arm_escape() {
        let mut state = State::default();
        state.observe(
            "item/started",
            &item("a", "1", "node_repl", "sky.click({})"),
        );
        assert!(state.targets().is_empty());
        state.observe("turn/started", &started("a", "1"));
        for (server, code) in [("node_repl", "1 + 1"), ("other", "sky.click({})")] {
            state.observe("item/started", &item("a", "1", server, code));
            assert!(state.targets().is_empty());
        }
        state.observe(
            "item/started",
            &item("a", "1", "node_repl", "await sky.click({x:1,y:2})"),
        );
        assert_eq!(state.targets(), vec![turn("a", "1")]);
    }

    #[test]
    fn native_pipe_calls_do_not_need_interrupt_markers() {
        let mut state = State::default();
        state.observe("turn/started", &started("a", "1"));
        state.observe(
            "item/started",
            &item("a", "1", "cua_repl", "await tab.click()"),
        );
        state.generation = 4;
        state.escape(4);
        assert_eq!(take(&mut state), vec![turn("a", "1")]);
        assert!(take(&mut state).is_empty());
        assert_eq!(state.targets(), vec![turn("a", "1")]); // ack is not completion
        assert_eq!(state.observe("turn/completed", &started("a", "1")).len(), 1);
        assert!(state.targets().is_empty());
    }

    #[test]
    fn stale_escape_and_terminal_events_cannot_stop_a_new_turn() {
        let mut state = State::default();
        state.observe("turn/started", &started("a", "1"));
        state.observe("item/started", &item("a", "1", "cua_repl", ""));
        state.generation = 1;
        state.observe("turn/started", &started("a", "2"));
        state.observe("item/started", &item("a", "2", "cua_repl", ""));
        state.generation = 2;
        assert!(
            state
                .observe("turn/completed", &started("a", "1"))
                .is_empty()
        );
        state.escape(1);
        state.escape(0);
        assert!(take(&mut state).is_empty());
        assert_eq!(state.targets(), vec![turn("a", "2")]);
    }

    #[test]
    fn escape_stops_only_active_computer_turns_and_can_retry() {
        let mut state = State::default();
        for thread in ["a", "b", "normal"] {
            state.observe("turn/started", &started(thread, "1"));
        }
        for thread in ["a", "b"] {
            state.observe("item/started", &item(thread, "1", "cua_repl", ""));
        }
        state.generation = 7;
        state.escape(7);
        assert_eq!(take(&mut state), vec![turn("a", "1"), turn("b", "1")]);
        assert!(take(&mut state).is_empty());
        state.observe("turn/completed", &started("a", "1"));
        state
            .turns
            .get_mut(&turn("b", "1"))
            .unwrap()
            .interrupt_in_flight = false;
        state.escape(7);
        assert_eq!(take(&mut state), vec![turn("b", "1")]);
    }

    #[test]
    fn exact_helper_stop_error_is_promoted_to_turn_interrupt() {
        let mut state = State::default();
        state.observe("turn/started", &started("a", "1"));
        let mut payload = item("a", "1", "node_repl", "alias.click({})");
        payload["item"]["result"] = json!({"content":[{"type":"text","text":format!("Error: {PHYSICAL_ESCAPE} Stop your work.")}]});
        state.observe("item/completed", &payload);
        assert_eq!(take(&mut state), vec![turn("a", "1")]);
        payload["item"]["server"] = json!("unrelated");
        assert!(!is_computer_stop(&payload["item"]));
        payload["item"]["server"] = json!("node_repl");
        payload["item"]["result"]["content"][0]["text"] =
            json!(format!("Documentation says: {PHYSICAL_ESCAPE}"));
        assert!(!is_computer_stop(&payload["item"]));
    }

    #[test]
    fn direct_helper_marker_is_scoped_and_never_deleted() {
        let home = std::env::temp_dir().join(format!("dvz-cu-markers-{}", std::process::id()));
        let target = turn("test-session", "old-turn");
        let marker = interrupt_marker(&home, &target).unwrap();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"").unwrap();
        let mut state = State::default();
        state.observe("turn/started", &started("test-session", "new-turn"));
        state.mark_interrupts(Some(&home));
        assert!(take(&mut state).is_empty());
        state.observe("turn/started", &started("test-session", "old-turn"));
        state.mark_interrupts(Some(&home));
        assert_eq!(take(&mut state), vec![target]);
        assert!(marker.is_file()); // the SDK's stop boundary must not be bypassed
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn marker_components_cannot_escape_the_cache() {
        let home = Path::new("home");
        assert!(interrupt_marker(home, &turn("..", "x")).is_none());
        assert!(interrupt_marker(home, &turn("x", ".")).is_none());
        assert_eq!(
            interrupt_marker(home, &turn("a/b", "c\\d🙂")).unwrap(),
            home.join("cache/computer-use/interrupts/a_b/c_d__")
        );
    }

    #[test]
    fn retrying_errors_leave_stop_armed_but_terminal_errors_disarm() {
        let mut state = State::default();
        state.observe("turn/started", &started("a", "1"));
        state.observe("item/started", &item("a", "1", "cua_repl", ""));
        assert!(
            state
                .observe(
                    "error",
                    &json!({"threadId":"a","turnId":"1","willRetry":true})
                )
                .is_empty()
        );
        assert!(!state.targets().is_empty());
        assert!(
            state
                .observe(
                    "error",
                    &json!({"threadId":"a","turnId":"1","willRetry":false})
                )
                .is_empty()
        );
        assert!(!state.targets().is_empty());
        assert_eq!(
            state
                .observe(
                    "turn/completed",
                    &json!({"threadId":"a","turn":{"id":"1","status":"failed"}})
                )
                .len(),
            1
        );
        assert!(state.targets().is_empty());
    }
    #[test]
    fn descendant_markers_use_authoritative_session_id_not_child_thread_id() {
        let home = std::env::temp_dir().join(format!("dvz-cu-child-marker-{}", std::process::id()));
        let marker = interrupt_marker(&home, &turn("root-session", "child-turn")).unwrap();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, b"").unwrap();
        let mut state = State::default();
        state.observe("thread/started", &json!({"thread":{"id":"child-thread","sessionId":"root-session","parentThreadId":"parent","cwd":"C:/work"}}));
        state.observe("turn/started", &started("child-thread", "child-turn"));
        state.mark_interrupts(Some(&home));
        assert_eq!(take(&mut state), vec![turn("child-thread", "child-turn")]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn late_thread_metadata_corrects_fallback_session_identity() {
        let mut state = State::default();
        state.observe("turn/started", &started("child", "1"));
        assert_eq!(
            state.turns[&turn("child", "1")].session.as_deref(),
            Some("child")
        );
        state.register_thread(
            &json!({"id":"child","sessionId":"root","cwd":"C:/work"}),
            None,
        );
        assert_eq!(
            state.turns[&turn("child", "1")].session.as_deref(),
            Some("root")
        );
    }

    #[test]
    fn thread_close_disarms_without_a_turn_id_and_keeps_cleanup_scope() {
        let mut state = State::default();
        state.register_thread(&json!({"id":"a","sessionId":"a","cwd":"C:/captured"}), None);
        state.observe("turn/started", &started("a", "1"));
        state.observe("item/started", &item("a", "1", "cua_repl", ""));
        let ended = state.observe("thread/closed", &json!({"threadId":"a"}));
        assert!(state.targets().is_empty());
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].turn, turn("a", "1"));
        assert_eq!(ended[0].cwd.as_deref(), Some("C:/captured"));
        assert!(ended[0].computer && ended[0].abnormal);
        assert!(
            state
                .observe("thread/closed", &json!({"threadId":"a"}))
                .is_empty()
        );
    }

    #[test]
    fn abnormal_cleanup_requires_terminal_status_and_is_deduplicated() {
        for (status, abnormal) in [
            ("completed", false),
            ("interrupted", true),
            ("failed", true),
        ] {
            let mut state = State::default();
            state.observe("turn/started", &started("a", "1"));
            state.observe("item/started", &item("a", "1", "cua_repl", ""));
            let event = json!({"threadId":"a","turn":{"id":"1","status":status}});
            let ended = state.observe("turn/completed", &event);
            assert_eq!(ended.len(), 1);
            assert_eq!(ended[0].abnormal, abnormal);
            assert!(state.observe("turn/completed", &event).is_empty());
        }
    }

    #[test]
    fn turn_cwd_is_captured_before_a_start_rpc_can_reply() {
        let mut state = State::default();
        state.register_thread(&json!({"id":"a","sessionId":"a","cwd":"C:/old"}), None);
        state.pending_cwd.insert("a".into(), "C:/new".into());
        state.observe("turn/started", &started("a", "1"));
        state.observe("item/started", &item("a", "1", "cua_repl", ""));
        let ended = state.observe(
            "turn/completed",
            &json!({"threadId":"a","turn":{"id":"1","status":"interrupted"}}),
        );
        assert_eq!(ended[0].cwd.as_deref(), Some("C:/new"));
    }
}
