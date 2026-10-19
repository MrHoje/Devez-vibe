use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::{app_server::ServerEvent, language::tr};

/// OpenCode provider 연동. /provider opencode와 /connect가 이 스위치를 따른다.
pub const PROVIDER_ENABLED: bool = true;

fn explicit_skill_instruction(input: &[Value]) -> Option<String> {
    let names = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("skill"))
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| {
        format!(
            "<devez-vibe-explicit-skills>\nThe user explicitly selected these OpenCode skills. Before handling the request, call the native skill tool once for each exact identifier (use the tool's id or name parameter): {}.\n</devez-vibe-explicit-skills>",
            names.join(", ")
        )
    })
}

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type Outbound = Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>;
type InboundMap = Arc<StdMutex<HashMap<String, InboundRequest>>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;
type DetachedMap = Arc<Mutex<HashMap<u64, DetachedTurn>>>;
type ToolMap = Arc<Mutex<HashMap<String, Value>>>;
type StreamMap = Arc<Mutex<HashMap<String, SessionStreams>>>;
type ActiveMap = Arc<Mutex<ActiveTurns>>;
type LoadingMap = Arc<Mutex<HashMap<String, LoadedHistory>>>;
type HistoryMap = Arc<Mutex<HashMap<String, Value>>>;
type Notification = (String, Value);

struct InboundRequest {
    id: Value,
    session_id: String,
}

/// 백엔드가 죽지 않고 응답만 멈추면 요청이 영원히 매달려 스피너만 남는다.
/// 세션 시작이나 큰 전사 복원도 이보다 오래 걸리지는 않으므로, 넘기면
/// 멈춘 것으로 보고 사용자에게 알린다.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Background child status is local OpenCode state, so polling once a second
/// removes a completed row promptly without adding external network traffic.
const BACKGROUND_SUBAGENT_STATUS_POLL: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct DetachedTurn {
    session_id: String,
    turn_id: String,
}

/// 한 세션에서 지금 돌고 있는 턴. 추가 지시가 같은 턴에 합류하도록 미결
/// prompt 수를 세고, 마지막 응답에서만 turn/completed를 발행한다.
struct ActiveTurn {
    turn_id: String,
    outstanding: usize,
}

#[derive(Default)]
struct ActiveTurns {
    active: HashMap<String, ActiveTurn>,
    /// Only prompt responses prove completion; absent sessions remain unknown.
    completed: VecDeque<DetachedTurn>,
}

const COMPLETED_TURN_LIMIT: usize = 128;
// 부모 결과보다 먼저 끝난 자식만 잠깐 보관한다.
const COMPLETED_CHILD_LIMIT: usize = 32;
const COMPLETED_CHILD_BYTES: usize = 4 * 1024 * 1024;

/// session/load가 응답 전에 재생해 주는 과거 대화 조각. 재개 화면 복원에 쓴다.
struct HistoryChunk {
    user: bool,
    message_id: Option<String>,
    text: String,
}

#[derive(Default)]
struct LoadedHistory {
    chunks: Vec<HistoryChunk>,
    token_usage: Option<Value>,
    plan: Option<Vec<Value>>,
}

/// ACP는 본문·사고 조각만 흘려보내고 항목 경계를 알리지 않는다. 조각을
/// 항목으로 묶어 시작·완료를 만들어 주지 않으면 한 턴의 모든 문장이 화면에서
/// 한 덩어리로 붙는다. 여기서 세션마다 열린 항목을 추적해 경계를 복원한다.
/// 사고 조각은 화면에 보내지 않는다. OpenCode는 요약이 아니라 사고 원문
/// 전체를 흘려보내서 그대로 표시하면 답변보다 길게 남는다. 대신 사고의
/// 시작은 앞 본문 항목이 끝났다는 경계 신호로만 쓴다.
#[derive(Default)]
struct SessionStreams {
    next_item: u64,
    message: Option<StreamItem>,
    /// 지금 돌고 있는 하위 에이전트(task 도구) 목록.
    subagents: Vec<Value>,
    /// 자식 알림에는 부모 호출 ID가 없어 도구 결과의 sessionID로 연결한다.
    child_tool_ids: HashMap<String, String>,
    child_items: Vec<Value>,
    child_root: Option<String>,
    child_completed_at: Option<Instant>,
}

struct StreamItem {
    id: String,
    source: Option<String>,
    text: String,
}

impl SessionStreams {
    fn record_child_update(
        &mut self,
        session_id: &str,
        update: &Value,
        tools: &mut HashMap<String, Value>,
    ) {
        let notifications = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => content_text(update.get("content"))
                .filter(|text| !text.is_empty())
                .map(|text| {
                    self.message_chunk(
                        session_id,
                        update.get("messageId").and_then(Value::as_str),
                        text,
                    )
                })
                .unwrap_or_default(),
            Some("agent_thought_chunk") => self.close_all(session_id),
            Some("tool_call" | "tool_call_update") => {
                let notifications = self.close_all(session_id);
                self.record_child_boundary(notifications);
                if let Some(id) = update.get("toolCallId").and_then(Value::as_str) {
                    let merged = merge_tool_update(tools.remove(id), update);
                    let completed = matches!(
                        merged.get("status").and_then(Value::as_str),
                        Some("completed" | "failed")
                    );
                    let item = tool_item(&merged, completed);
                    if let Some(existing) = self
                        .child_items
                        .iter_mut()
                        .find(|existing| existing.get("id") == item.get("id"))
                    {
                        *existing = item;
                    } else {
                        self.child_items.push(item);
                    }
                    if !completed {
                        tools.insert(id.to_owned(), merged);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.record_child_boundary(notifications);
    }

    fn record_child_boundary(&mut self, notifications: Vec<Notification>) {
        for (method, params) in notifications {
            if method == "item/completed"
                && let Some(item) = params.get("item")
            {
                self.child_items.push(item.clone());
            }
        }
        if self.child_items.len() > 400 {
            self.child_items.drain(..self.child_items.len() - 400);
        }
    }

    fn child_history(&self, session_id: &str, tool_id: &str) -> Option<Notification> {
        let mut items = self.child_items.clone();
        if let Some(message) = &self.message {
            items.push(json!({ "id": message.id, "type": "agentMessage", "text": message.text, "provider": "OpenCode" }));
        }
        (!items.is_empty()).then(|| {
            (
                "turn/subagent/history".to_owned(),
                json!({ "threadId": session_id, "id": tool_id, "turns": [{ "items": items }] }),
            )
        })
    }

    fn message_chunk(
        &mut self,
        session_id: &str,
        message_id: Option<&str>,
        delta: &str,
    ) -> Vec<Notification> {
        let mut out = Vec::new();
        if message_id.is_some()
            && self
                .message
                .as_ref()
                .is_some_and(|current| current.source.as_deref() != message_id)
        {
            self.close_message(session_id, &mut out);
        }
        if self.message.is_none() {
            self.next_item += 1;
            let item = StreamItem {
                id: format!("opencode-message-{}", self.next_item),
                source: message_id.map(ToOwned::to_owned),
                text: String::new(),
            };
            out.push((
                "item/started".to_owned(),
                json!({
                    "threadId": session_id,
                    "item": {
                        "id": item.id,
                        "type": "agentMessage",
                        "text": "",
                        "provider": "OpenCode"
                    }
                }),
            ));
            self.message = Some(item);
        }
        let current = self.message.as_mut().expect("message stream exists");
        current.text.push_str(delta);
        out.push((
            "item/agentMessage/delta".to_owned(),
            json!({
                "threadId": session_id,
                "itemId": current.id,
                "delta": delta,
                "provider": "OpenCode"
            }),
        ));
        out
    }

    /// 사고는 다음 응답의 서두이므로 앞의 본문 항목은 여기서 끝난다.
    /// 사고 내용 자체는 표시하지 않는다.
    fn thought_boundary(&mut self, session_id: &str) -> Vec<Notification> {
        let mut out = Vec::new();
        self.close_message(session_id, &mut out);
        out
    }

    /// 도구 호출 시작과 턴 종료가 항목이 끝났다고 확신할 수 있는 경계다.
    fn close_all(&mut self, session_id: &str) -> Vec<Notification> {
        let mut out = Vec::new();
        self.close_message(session_id, &mut out);
        out
    }

    fn close_message(&mut self, session_id: &str, out: &mut Vec<Notification>) {
        if let Some(current) = self.message.take() {
            out.push((
                "item/completed".to_owned(),
                json!({
                    "threadId": session_id,
                    "item": {
                        "id": current.id,
                        "type": "agentMessage",
                        "text": current.text,
                        "provider": "OpenCode"
                    }
                }),
            ));
        }
    }

    /// OpenCode의 task 도구는 하위 에이전트다. 실행 목록을 유지해 화면의
    /// 하위 에이전트 행으로 알린다.
    fn subagent_started(&mut self, session_id: &str, update: &Value) -> Option<Notification> {
        let entry = subagent_entry(update)?;
        let id = entry.get("id").and_then(Value::as_str)?.to_owned();
        self.subagents
            .retain(|existing| existing.get("id").and_then(Value::as_str) != Some(id.as_str()));
        self.subagents.push(entry);
        Some(self.subagents_notification(session_id))
    }

    fn subagent_finished(&mut self, session_id: &str, tool_id: &str) -> Option<Notification> {
        let before = self.subagents.len();
        self.subagents
            .retain(|existing| existing.get("id").and_then(Value::as_str) != Some(tool_id));
        (self.subagents.len() != before).then(|| self.subagents_notification(session_id))
    }

    /// OpenCode 2 starts the subagent call with empty input and names the agent
    /// in a later update, so the row takes its name from there.
    fn subagent_described(&mut self, session_id: &str, update: &Value) -> Option<Notification> {
        if update.get("name").and_then(Value::as_str) != Some("subagent") {
            return None;
        }
        let described = subagent_entry(update)?;
        let entry = self
            .subagents
            .iter_mut()
            .find(|entry| entry.get("id") == described.get("id"))?;
        if entry.get("name") == described.get("name")
            && entry.get("description") == described.get("description")
        {
            return None;
        }
        entry["name"] = described["name"].clone();
        entry["description"] = described["description"].clone();
        Some(self.subagents_notification(session_id))
    }

    /// OpenCode 2 reports a background child's end instead of answering status polls.
    fn background_subagent_ended(
        &mut self,
        session_id: &str,
        task_id: &str,
    ) -> Option<Notification> {
        let before = self.subagents.len();
        self.subagents
            .retain(|entry| entry.get("taskId").and_then(Value::as_str) != Some(task_id));
        (self.subagents.len() != before).then(|| self.subagents_notification(session_id))
    }

    /// OpenCode의 background task는 바깥 tool call이 끝나도 자식 세션이 계속
    /// 돈다. 순정 tool metadata의 sessionId를 붙여 status API와 대조한다.
    fn subagent_tool_completed(
        &mut self,
        session_id: &str,
        tool_id: &str,
        update: &Value,
    ) -> Option<Notification> {
        let Some(task_id) = background_subagent_task_id(update) else {
            return self.subagent_finished(session_id, tool_id);
        };
        let entry = self
            .subagents
            .iter_mut()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(tool_id))?;
        entry["background"] = Value::Bool(true);
        entry["taskId"] = Value::String(task_id.to_owned());
        entry["missingPolls"] = json!(0);
        entry["observedActive"] = Value::Bool(false);
        Some(self.subagents_notification(session_id))
    }

    /// 턴이 끝나면 foreground 행만 거둔다. background 행은 자식 세션 status가
    /// idle이 되거나 연결이 닫힐 때까지 살아 있어야 한다.
    fn subagents_cleared(&mut self, session_id: &str) -> Option<Notification> {
        let before = self.subagents.len();
        self.subagents
            .retain(|entry| entry.get("background").and_then(Value::as_bool) == Some(true));
        (self.subagents.len() != before).then(|| self.subagents_notification(session_id))
    }

    fn all_subagents_cleared(&mut self, session_id: &str) -> Option<Notification> {
        if self.subagents.is_empty() {
            return None;
        }
        self.subagents.clear();
        Some(self.subagents_notification(session_id))
    }

    fn has_background_subagents(&self) -> bool {
        self.subagents
            .iter()
            .any(|entry| entry.get("background").and_then(Value::as_bool) == Some(true))
    }

    /// `/session/status` omits idle sessions. Before the child is observed, two
    /// missing snapshots absorb the launch race. Once it has been active, the
    /// first missing or explicit idle snapshot is authoritative completion.
    fn reconcile_background_subagents(
        &mut self,
        session_id: &str,
        statuses: &Value,
    ) -> Option<Notification> {
        let mut changed = false;
        let mut retained_background = false;
        self.subagents.retain_mut(|entry| {
            if entry.get("background").and_then(Value::as_bool) != Some(true) {
                return true;
            }
            let task_id = entry
                .get("taskId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let status = (!task_id.is_empty())
                .then(|| statuses.get(task_id))
                .flatten()
                .and_then(|status| status.get("type"))
                .and_then(Value::as_str);
            if status.is_some_and(|status| status != "idle") {
                entry["missingPolls"] = json!(0);
                entry["observedActive"] = Value::Bool(true);
                retained_background = true;
                return true;
            }
            if status == Some("idle")
                || entry.get("observedActive").and_then(Value::as_bool) == Some(true)
            {
                changed = true;
                return false;
            }
            let missing = entry
                .get("missingPolls")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                + 1;
            entry["missingPolls"] = json!(missing);
            if missing < 2 {
                retained_background = true;
                true
            } else {
                changed = true;
                false
            }
        });
        (changed || retained_background).then(|| self.subagents_notification(session_id))
    }

    fn subagents_notification(&self, session_id: &str) -> Notification {
        (
            "turn/subagents/updated".to_owned(),
            json!({ "threadId": session_id, "subagents": self.subagents }),
        )
    }
}

/// task 도구 호출에서 하위 에이전트 행 하나를 만든다. task가 아니면 None.
fn subagent_entry(update: &Value) -> Option<Value> {
    let input = update.get("rawInput");
    // OpenCode 2 names the tool `subagent` and its agent input `agent`.
    let v2 = update.get("name").and_then(Value::as_str) == Some("subagent");
    let subagent_type = input
        .and_then(|input| input.get(if v2 { "agent" } else { "subagent_type" }))
        .and_then(Value::as_str);
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !v2 && subagent_type.is_none() && !title.eq_ignore_ascii_case("task") {
        return None;
    }
    let id = update.get("toolCallId").and_then(Value::as_str)?;
    let description = input
        .and_then(|input| input.get("description"))
        .and_then(Value::as_str)
        .unwrap_or(title);
    Some(json!({
        "id": id,
        "name": subagent_type.unwrap_or("agent"),
        "description": description,
        "tool": "Task",
        "background": false
    }))
}

fn background_subagent_task_id(update: &Value) -> Option<&str> {
    let metadata = update.pointer("/rawOutput/metadata")?;
    // OpenCode 2 reports a subagent that keeps running as `status: "running"`.
    if metadata.get("status").and_then(Value::as_str) == Some("running") {
        return metadata
            .get("sessionID")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
    }
    (metadata.get("background").and_then(Value::as_bool) == Some(true))
        .then_some(())
        .and_then(|()| {
            metadata
                .get("sessionId")
                .or_else(|| metadata.get("jobId"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
}

#[derive(Clone)]
pub struct OpenCodeClient {
    outbound: Outbound,
    inbound: InboundMap,
    pending: PendingMap,
    detached: DetachedMap,
    active_turns: ActiveMap,
    next_id: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<ServerEvent>,
}

impl OpenCodeClient {
    async fn turn_status(&self, session_id: &str, expected_turn: Option<&str>) -> Value {
        let turns = self.active_turns.lock().await;
        match turns.active.get(session_id) {
            Some(turn) if turn.outstanding > 0 => {
                json!({ "known": true, "running": true, "turnId": turn.turn_id })
            }
            _ => match expected_turn.filter(|id| !id.is_empty()).and_then(|id| {
                turns
                    .completed
                    .iter()
                    .find(|turn| turn.session_id == session_id && turn.turn_id == id)
            }) {
                Some(turn) => json!({ "known": true, "running": false, "turnId": turn.turn_id }),
                _ => json!({ "known": false }),
            },
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);
        if let Err(error) = self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        })) {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match timeout(REQUEST_TIMEOUT, response_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("{method}: {error}"),
            Ok(Err(_)) => bail!(tr(
                format!("{method}: OpenCode ACP 응답 채널이 종료되었습니다."),
                format!("{method}: the OpenCode ACP response channel closed."),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let minutes = REQUEST_TIMEOUT.as_secs() / 60;
                bail!(tr(
                    format!("{method}: OpenCode가 {minutes}분 동안 응답하지 않아 요청을 중단했습니다."),
                    format!("{method}: stopped the request because OpenCode didn't answer for {minutes} minutes."),
                ))
            }
        }
    }

    pub async fn start_prompt(
        &self,
        session_id: &str,
        prompt: Vec<Value>,
        steer: bool,
    ) -> Result<String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // 추가 지시는 이미 도는 턴에 합류한다. 새 턴을 만들면 스피너와 항목
        // 추적이 턴이 바뀐 것으로 읽고 진행 표시가 끊긴다.
        let (turn_id, joined) = {
            let mut active = self.active_turns.lock().await;
            match active.active.get_mut(session_id) {
                Some(turn) if steer => {
                    turn.outstanding += 1;
                    (turn.turn_id.clone(), true)
                }
                _ => {
                    let turn_id = format!(
                        "opencode-turn-{}",
                        self.next_turn.fetch_add(1, Ordering::Relaxed)
                    );
                    active.active.insert(
                        session_id.to_owned(),
                        ActiveTurn {
                            turn_id: turn_id.clone(),
                            outstanding: 1,
                        },
                    );
                    (turn_id, false)
                }
            }
        };
        self.detached.lock().await.insert(
            id,
            DetachedTurn {
                session_id: session_id.to_owned(),
                turn_id: turn_id.clone(),
            },
        );
        if let Err(error) = self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": prompt
            }
        })) {
            self.detached.lock().await.remove(&id);
            let mut active = self.active_turns.lock().await;
            if let Some(turn) = active.active.get_mut(session_id)
                && turn.turn_id == turn_id
            {
                turn.outstanding = turn.outstanding.saturating_sub(1);
                if turn.outstanding == 0 {
                    active.active.remove(session_id);
                }
            }
            return Err(error);
        }
        if !joined {
            let _ = self.events.send(ServerEvent::Notification {
                method: "turn/started".to_owned(),
                params: json!({
                    "threadId": session_id,
                    "turn": { "id": turn_id }
                }),
            });
        }
        Ok(turn_id)
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        }))
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        let result = if id.get("kind").and_then(Value::as_str) == Some("elicitation") {
            result
        } else {
            permission_result(&result)
        };
        let id = open_code_request_id(&id)?;
        self.inbound
            .lock()
            .expect("inbound mutex")
            .remove(&id.to_string());
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        let id = open_code_request_id(&id)?;
        self.inbound
            .lock()
            .expect("inbound mutex")
            .remove(&id.to_string());
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }))
    }

    fn close(&self) {
        cancel_inbound_requests(&self.inbound, &self.outbound, &self.events, None);
        self.outbound.lock().expect("outbound mutex").take();
    }

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .lock()
            .expect("outbound mutex")
            .as_ref()
            .ok_or_else(|| {
                anyhow!(tr("OpenCode ACP 연결이 이미 종료되었습니다.", "The OpenCode ACP connection has already closed."))
            })?
            .send(message)
            .map_err(|_| anyhow!(tr("OpenCode ACP에 메시지를 보낼 수 없습니다.", "Can't send a message to OpenCode ACP.")))
    }
}

pub struct OpenCodeServer {
    child: Child,
    /// `.cmd` 런처가 감싼 실제 OpenCode 프로세스까지 함께 끝낸다.
    _job: Option<crate::child_process::BackendJob>,
    client: OpenCodeClient,
    provider_auth: ProviderAuthServer,
    /// 2.x provider API server, which ends when its stdin closes.
    helper: Option<Child>,
    _helper_job: Option<crate::child_process::BackendJob>,
    /// session/load가 진행 중인 세션의 재생 조각 버퍼.
    loading: LoadingMap,
    /// 마지막 session/load가 재생해 준 과거 대화 페이지.
    history: HistoryMap,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    subagent_task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct ProviderAuthServer {
    client: reqwest::Client,
    base_url: reqwest::Url,
    /// Only the OpenCode 2 server has one, and every 2.x route requires it.
    password: Option<String>,
    /// OpenCode 2 OAuth attempts by provider, from authorize until callback.
    oauth_attempts: Arc<StdMutex<HashMap<String, String>>>,
}

impl ProviderAuthServer {
    fn new(port: u16) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            base_url: reqwest::Url::parse(&format!("http://127.0.0.1:{port}/"))?,
            password: None,
            oauth_attempts: Arc::default(),
        })
    }

    /// OpenCode 2's `acp` keeps its own server's address and password private,
    /// so provider, model, and skill requests go to a second server on the same
    /// database. It runs until its stdin closes.
    async fn spawn_v2(
        command: &Path,
        cwd: &Path,
    ) -> Result<(Self, Child, Option<crate::child_process::BackendJob>)> {
        let password = random_password();
        let mut server = command_for(command);
        crate::child_process::isolate_backend(&mut server);
        server
            .args(["serve", "--stdio", "--port", "0"])
            .current_dir(cwd)
            .env("OPENCODE_PASSWORD", &password)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = server.spawn().context(tr(
            "OpenCode provider API 서버를 시작하지 못했습니다.",
            "Couldn't start the OpenCode provider API server.",
        ))?;
        let job = crate::child_process::adopt_backend(&child);
        let stdout = child.stdout.take().context(tr(
            "OpenCode provider API 서버 stdout 연결 실패",
            "OpenCode provider API server stdout connection failed",
        ))?;
        let mut lines = BufReader::new(stdout).lines();
        let url = timeout(Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                let url = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|line| line.get("url")?.as_str().map(ToOwned::to_owned));
                if url.is_some() {
                    return url;
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .context(tr(
            "OpenCode provider API 서버 주소를 받지 못했습니다.",
            "Didn't receive the OpenCode provider API server address.",
        ))?;
        // Reading on keeps later output from filling the pipe and stalling the server.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let server = Self {
            client: reqwest::Client::new(),
            base_url: reqwest::Url::parse(&format!("{url}/"))?,
            password: Some(password),
            oauth_attempts: Arc::default(),
        };
        Ok((server, child, job))
    }

    fn is_v2(&self) -> bool {
        self.password.is_some()
    }

    async fn wait_until_ready(&self) -> Result<()> {
        for _ in 0..60 {
            let request = if self.is_v2() {
                self.v2_request(reqwest::Method::GET, &["api", "info"])?
            } else {
                self.client.get(self.url(&["global", "health"])?)
            }
            .timeout(Duration::from_secs(2));
            if request
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!(tr(
            "OpenCode provider API가 제한 시간 안에 준비되지 않았습니다.",
            "The OpenCode provider API wasn't ready within the time limit.",
        ))
    }

    pub async fn catalog(&self) -> Result<Value> {
        if self.is_v2() {
            return self.v2_catalog().await;
        }
        self.wait_until_ready().await?;
        let (providers, auth) =
            tokio::try_join!(self.get(&["provider"]), self.get(&["provider", "auth"]))?;
        Ok(json!({
            "all": providers.get("all").cloned().unwrap_or_else(|| json!([])),
            "connected": providers
                .get("connected")
                .cloned()
                .unwrap_or_else(|| json!([])),
            "auth": auth
        }))
    }

    async fn model_catalog(&self) -> Result<Value> {
        self.wait_until_ready().await?;
        self.get(&["provider"]).await
    }

    pub async fn skills(&self, cwd: &Path) -> Result<Value> {
        let directory = cwd.to_string_lossy();
        if self.is_v2() {
            let skills = self
                .v2_list(&["api", "skill"], &[("location[directory]", &directory)])
                .await?;
            return Ok(normalize_skill_catalog(Value::Array(skills)));
        }
        self.wait_until_ready().await?;
        Ok(normalize_skill_catalog(
            self.send_json(
                self.client
                    .get(self.url(&["skill"])?)
                    .query(&[("directory", &directory)])
                    .timeout(Duration::from_secs(30)),
            )
            .await?,
        ))
    }

    async fn session_status(&self) -> Result<Value> {
        self.get(&["session", "status"]).await
    }

    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<Value> {
        self.wait_until_ready().await?;
        let request = if self.is_v2() {
            self.v2_request(reqwest::Method::PATCH, &["api", "session", session_id])?
        } else {
            self.client
                .patch(self.url(&["session", session_id])?)
                .timeout(Duration::from_secs(30))
        };
        self.send_json(request.json(&json!({ "title": name })))
            .await
    }

    pub async fn set_api_key(
        &self,
        provider_id: &str,
        key: &str,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        if self.is_v2() {
            let mut body = json!({ "key": key });
            if !inputs.is_empty() {
                body["answer"] = serde_json::to_value(inputs)?;
            }
            let path = ["api", "integration", provider_id, "connect", "key"];
            self.send_json(self.v2_request(reqwest::Method::POST, &path)?.json(&body))
                .await?;
            return Ok(());
        }
        let mut body = json!({ "type": "api", "key": key });
        if !inputs.is_empty() {
            body["metadata"] = serde_json::to_value(inputs)?;
        }
        self.send_json(
            self.client
                .put(self.url(&["auth", provider_id])?)
                .timeout(Duration::from_secs(30))
                .json(&body),
        )
        .await?;
        Ok(())
    }

    pub async fn oauth_authorize(
        &self,
        provider_id: &str,
        method: usize,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        if self.is_v2() {
            return self.v2_oauth_authorize(provider_id, method, inputs).await;
        }
        self.send_json(
            self.client
                .post(self.url(&["provider", provider_id, "oauth", "authorize"])?)
                .timeout(Duration::from_secs(60))
                .json(&json!({
                    "method": method,
                    "inputs": inputs
                })),
        )
        .await
    }

    pub async fn oauth_callback(
        &self,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<()> {
        if self.is_v2() {
            return self.v2_oauth_callback(provider_id, code).await;
        }
        self.send_json(
            self.client
                .post(self.url(&["provider", provider_id, "oauth", "callback"])?)
                .timeout(Duration::from_secs(600))
                .json(&json!({
                    "method": method,
                    "code": code
                })),
        )
        .await?;
        Ok(())
    }

    async fn get(&self, path: &[&str]) -> Result<Value> {
        self.send_json(
            self.client
                .get(self.url(path)?)
                .timeout(Duration::from_secs(30)),
        )
        .await
    }

    async fn send_json(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let response = request
            .send()
            .await
            .context(tr("OpenCode provider API 요청 실패", "OpenCode provider API request failed"))?;
        let status = response.status();
        let body = response.text().await.context(tr(
            "OpenCode provider API 응답 읽기 실패",
            "Couldn't read the OpenCode provider API response",
        ))?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/data/message")
                        .or_else(|| value.get("message"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .unwrap_or(body);
            bail!("OpenCode provider API {status}: {detail}");
        }
        if body.trim().is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_str(&body)
                .context(tr("OpenCode provider API 응답 해석 실패", "Couldn't parse the OpenCode provider API response"))
        }
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow!(tr("OpenCode provider API URL을 만들 수 없습니다.", "Can't build the OpenCode provider API URL.")))?
            .extend(segments);
        Ok(url)
    }

    fn v2_request(
        &self,
        method: reqwest::Method,
        segments: &[&str],
    ) -> Result<reqwest::RequestBuilder> {
        Ok(self
            .client
            .request(method, self.url(segments)?)
            .basic_auth("opencode", self.password.as_deref())
            .timeout(Duration::from_secs(30)))
    }

    /// 2.x lists arrive as `{ location, data }`.
    async fn v2_list(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Vec<Value>> {
        self.wait_until_ready().await?;
        let response = self
            .send_json(
                self.v2_request(reqwest::Method::GET, segments)?
                    .query(query),
            )
            .await?;
        Ok(response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// 2.x integrations are the connectable providers, and their methods take
    /// the place of the 1.x `/provider/auth` entries.
    async fn v2_catalog(&self) -> Result<Value> {
        let mut all = Vec::new();
        let mut connected = Vec::new();
        let mut auth = Map::new();
        for integration in self.v2_list(&["api", "integration"], &[]).await? {
            let Some(id) = integration.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = integration
                .get("name")
                .cloned()
                .unwrap_or_else(|| json!(id));
            all.push(json!({ "id": id, "name": name }));
            if integration
                .get("connections")
                .and_then(Value::as_array)
                .is_some_and(|connections| !connections.is_empty())
            {
                connected.push(json!(id));
            }
            let methods = integration
                .get("methods")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(v2_auth_method)
                .collect();
            auth.insert(id.to_owned(), Value::Array(methods));
        }
        Ok(json!({ "all": all, "connected": connected, "auth": auth }))
    }

    /// 2.x keeps the model list current on its server, while the running `acp`
    /// only reloads its own copy for changes made through its private server.
    async fn v2_models(&self) -> Result<Value> {
        let mut models = self.v2_list(&["api", "model"], &[]).await?;
        models.retain(|model| {
            model.get("enabled").and_then(Value::as_bool) == Some(true)
                && model.get("providerID").and_then(Value::as_str) != Some("openai")
        });
        let field = |model: &Value, key: &str| {
            model
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        models.sort_by_cached_key(|model| {
            (
                field(model, "providerID"),
                field(model, "name").to_lowercase(),
            )
        });
        let data = models
            .iter()
            .map(|model| {
                let provider = field(model, "providerID");
                let mut efforts = model
                    .get("variants")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|variant| variant.get("id")?.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>();
                efforts.sort_by_key(|effort| effort_rank(effort));
                open_code_model(
                    &format!("{provider}/{}", field(model, "id")),
                    &format!("{provider}/{}", field(model, "name")),
                    &efforts,
                )
            })
            .collect::<Vec<_>>();
        Ok(json!({ "data": data }))
    }

    /// 2.x `session/list` also returns subagent sessions; the server can tell
    /// the top-level ones apart.
    async fn v2_root_sessions(&self, cwd: Option<&Path>) -> Result<Vec<String>> {
        let directory = cwd.map(|cwd| cwd.to_string_lossy().into_owned());
        let mut query = vec![("parentID", "null"), ("order", "desc"), ("limit", "100")];
        if let Some(directory) = &directory {
            query.push(("directory", directory));
        }
        Ok(self
            .v2_list(&["api", "session"], &query)
            .await?
            .iter()
            .filter_map(|session| session.get("id")?.as_str().map(ToOwned::to_owned))
            .collect())
    }

    /// The picker sends a method's position, but 2.x addresses OAuth methods
    /// by id and keeps the attempt on this server until it completes.
    async fn v2_oauth_authorize(
        &self,
        provider_id: &str,
        method: usize,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        let integrations = self.v2_list(&["api", "integration"], &[]).await?;
        let method_id = integrations
            .iter()
            .find(|integration| integration.get("id").and_then(Value::as_str) == Some(provider_id))
            .and_then(|integration| integration.pointer(&format!("/methods/{method}/id")))
            .and_then(Value::as_str)
            .context(tr(
                "OpenCode 인증 방식을 찾지 못했습니다.",
                "Couldn't find the OpenCode sign-in method.",
            ))?;
        let mut body = json!({ "methodID": method_id });
        if !inputs.is_empty() {
            body["answer"] = serde_json::to_value(inputs)?;
        }
        let path = ["api", "integration", provider_id, "connect", "oauth"];
        let response = self
            .send_json(
                self.v2_request(reqwest::Method::POST, &path)?
                    .timeout(Duration::from_secs(60))
                    .json(&body),
            )
            .await?;
        let attempt = response
            .pointer("/data/attemptID")
            .and_then(Value::as_str)
            .context(tr(
                "OpenCode 인증 시도 id가 없습니다.",
                "The OpenCode sign-in attempt has no id.",
            ))?;
        self.oauth_attempts
            .lock()
            .expect("OpenCode OAuth attempts")
            .insert(provider_id.to_owned(), attempt.to_owned());
        Ok(json!({
            "url": response.pointer("/data/url"),
            "method": response.pointer("/data/mode"),
            "instructions": response.pointer("/data/instructions")
        }))
    }

    /// A pasted code completes the attempt; a browser sign-in finishes on the
    /// server's side, so the attempt is polled until it settles.
    async fn v2_oauth_callback(&self, provider_id: &str, code: Option<&str>) -> Result<()> {
        let attempt = self
            .oauth_attempts
            .lock()
            .expect("OpenCode OAuth attempts")
            .get(provider_id)
            .cloned()
            .context(tr(
                "진행 중인 OpenCode 인증이 없습니다.",
                "There's no OpenCode sign-in in progress.",
            ))?;
        let path = [
            "api",
            "integration",
            provider_id,
            "connect",
            "oauth",
            &attempt,
        ];
        if let Some(code) = code {
            let complete = [&path[..], &["complete"]].concat();
            self.send_json(
                self.v2_request(reqwest::Method::POST, &complete)?
                    .json(&json!({ "code": code })),
            )
            .await?;
            return Ok(());
        }
        for _ in 0..600 {
            let state = self
                .send_json(self.v2_request(reqwest::Method::GET, &path)?)
                .await?;
            match state.pointer("/data/status").and_then(Value::as_str) {
                Some("complete") => return Ok(()),
                Some("pending") => sleep(Duration::from_secs(1)).await,
                status => bail!(
                    state
                        .pointer("/data/message")
                        .and_then(Value::as_str)
                        .or(status)
                        .unwrap_or("failed")
                        .to_owned()
                ),
            }
        }
        bail!(tr(
            "OpenCode 인증이 제한 시간 안에 끝나지 않았습니다.",
            "The OpenCode sign-in didn't finish within the time limit.",
        ))
    }
}

/// 2.x auth methods in the 1.x picker shape. `env`, `command`, and `external`
/// keep their own types, which the picker skips, so positions stay aligned.
fn v2_auth_method(method: &Value) -> Value {
    let kind = match method.get("type").and_then(Value::as_str) {
        Some("key") => "api",
        other => other.unwrap_or_default(),
    };
    let label = method
        .get("label")
        .cloned()
        .unwrap_or_else(|| json!("API key"));
    let prompts = method
        .get("form")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|field| field.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|field| {
            let key = field.get("key")?.as_str()?;
            let message = field
                .get("title")
                .or_else(|| field.get("description"))
                .and_then(Value::as_str)
                .unwrap_or(key);
            // The picker shows one condition; 2.x forms use one in practice.
            let when = field.pointer("/when/0").map(|when| {
                let value = when.get("value").map(|value| match value {
                    Value::String(value) => value.clone(),
                    value => value.to_string(),
                });
                json!({ "key": when.get("key"), "op": when.get("op"), "value": value })
            });
            Some(match field.get("options").and_then(Value::as_array) {
                Some(options) if !options.is_empty() => json!({
                    "type": "select",
                    "key": key,
                    "message": message,
                    "when": when,
                    "options": options
                        .iter()
                        .map(|option| json!({
                            "label": option.get("label"),
                            "value": option.get("value"),
                            "hint": option.get("description")
                        }))
                        .collect::<Vec<_>>()
                }),
                _ => json!({
                    "type": "text",
                    "key": key,
                    "message": message,
                    "placeholder": field.get("placeholder"),
                    "when": when
                }),
            })
        })
        .collect::<Vec<_>>();
    json!({ "type": kind, "label": label, "prompts": prompts })
}

/// The 2.x server listens on loopback only; its password keeps other local
/// programs from using it. `RandomState` keys come from the OS generator.
fn random_password() -> String {
    use std::hash::{BuildHasher, RandomState};
    (0..4)
        .map(|part| format!("{:016x}", RandomState::new().hash_one(part)))
        .collect()
}

/// OpenCode 2 dropped the 1.x `acp` flags and HTTP routes. `--version` prints
/// `1.19.0` on 1.x and `opencode v2.0.27` on 2.x.
async fn is_open_code_v2(command: &Path) -> bool {
    let mut probe = command_for(command);
    crate::child_process::isolate_backend(&mut probe);
    probe
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(output)) = timeout(Duration::from_secs(10), probe.output()).await else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 2)
}

fn normalize_skill_catalog(response: Value) -> Value {
    let skills = response
        .as_array()
        .or_else(|| response.get("data").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|skill| {
            let name = skill.get("id").and_then(Value::as_str)
                .or_else(|| skill.get("name").and_then(Value::as_str))?;
            // 2.x names the field `path`.
            let location = skill
                .get("location")
                .or_else(|| skill.get("path"))
                .and_then(Value::as_str);
            let path = location
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("opencode://skill/{name}"));
            Some(json!({
                "name": name,
                "path": path,
                "description": skill
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "enabled": true,
                "scope": open_code_skill_scope(location)
            }))
        })
        .collect::<Vec<_>>();
    json!({ "data": [{ "skills": skills, "errors": [] }] })
}

fn open_code_skill_scope(location: Option<&str>) -> &'static str {
    let Some(path) = location.map(Path::new) else {
        return "system";
    };
    let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) else {
        return "provider";
    };
    let home = PathBuf::from(home);
    let config = env::var_os("OPENCODE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|root| PathBuf::from(root).join("opencode")))
        .unwrap_or_else(|| home.join(".config").join("opencode"));
    [
        config.join("skills"),
        home.join(".claude").join("skills"),
        home.join(".agents").join("skills"),
    ]
    .iter()
    .any(|root| path.starts_with(root))
    .then_some("user")
    .unwrap_or("provider")
}

impl OpenCodeServer {
    pub async fn spawn(open_code_path: &Path, cwd: &Path) -> Result<Self> {
        let resolved = resolve_command(open_code_path);
        let v2 = is_open_code_v2(&resolved).await;
        let mut command = command_for(&resolved);
        crate::child_process::isolate_backend(&mut command);
        let port = if v2 {
            // 2.x `acp` takes no flags; each session request names its directory.
            command.arg("acp").current_dir(cwd);
            None
        } else {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .context(tr("OpenCode provider API 포트를 확보하지 못했습니다.", "Couldn't reserve a port for the OpenCode provider API."))?;
            let port = listener.local_addr()?.port();
            drop(listener);
            command
                .args([
                    "acp",
                    "--hostname",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                    "--cwd",
                ])
                .arg(cwd);
            Some(port)
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            let resolved = resolved.display();
            tr(
                format!("OpenCode ACP를 시작하지 못했습니다: {resolved}"),
                format!("Couldn't start OpenCode ACP: {resolved}"),
            )
        })?;
        let job = crate::child_process::adopt_backend(&child);
        let stdin = child.stdin.take().context(tr("OpenCode ACP stdin 연결 실패", "OpenCode ACP stdin connection failed"))?;
        let stdout = child
            .stdout
            .take()
            .context(tr("OpenCode ACP stdout 연결 실패", "OpenCode ACP stdout connection failed"))?;
        let stderr = child
            .stderr
            .take()
            .context(tr("OpenCode ACP stderr 연결 실패", "OpenCode ACP stderr connection failed"))?;

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        let outbound = Arc::new(StdMutex::new(Some(outbound_tx)));
        let (event_tx, events) = mpsc::unbounded_channel::<ServerEvent>();
        let inbound: InboundMap = Arc::default();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let detached: DetachedMap = Arc::new(Mutex::new(HashMap::new()));
        let active_turns: ActiveMap = Arc::default();
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let (provider_auth, helper, helper_job) = match port {
            Some(port) => (ProviderAuthServer::new(port)?, None, None),
            None => {
                let (server, helper, job) = ProviderAuthServer::spawn_v2(&resolved, cwd).await?;
                (server, Some(helper), job)
            }
        };

        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = outbound_rx.recv().await {
                let mut encoded = match serde_json::to_vec(&message) {
                    Ok(encoded) => encoded,
                    Err(_) => continue,
                };
                encoded.push(b'\n');
                if stdin.write_all(&encoded).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

        let reader_pending = Arc::clone(&pending);
        let reader_outbound = Arc::clone(&outbound);
        let reader_inbound = Arc::clone(&inbound);
        let reader_detached = Arc::clone(&detached);
        let reader_active = Arc::clone(&active_turns);
        let reader_loading = Arc::clone(&loading);
        let reader_tools = Arc::clone(&tools);
        let reader_streams = Arc::clone(&streams);
        let reader_events = event_tx.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        match serde_json::from_str::<Value>(&line) {
                            Ok(message) => {
                                route_message(
                                    message,
                                    &reader_pending,
                                    &reader_detached,
                                    &reader_active,
                                    &reader_loading,
                                    &reader_tools,
                                    &reader_streams,
                                    &reader_events,
                                    &reader_inbound,
                                    &reader_outbound,
                                )
                                .await;
                            }
                            Err(error) => {
                                let _ = reader_events.send(ServerEvent::ProtocolWarning(tr(
                                    format!("OpenCode ACP JSON 해석 실패: {error}"),
                                    format!("Couldn't parse OpenCode ACP JSON: {error}"),
                                )));
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events.send(ServerEvent::ProtocolWarning(tr(
                            format!("OpenCode ACP 출력 읽기 실패: {error}"),
                            format!("Couldn't read OpenCode ACP output: {error}"),
                        )));
                        break;
                    }
                }
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(tr("OpenCode ACP 연결이 종료되었습니다.", "The OpenCode ACP connection closed.").to_owned()));
            }
            drop(pending);
            cancel_inbound_requests(&reader_inbound, &reader_outbound, &reader_events, None);
            // 연결이 죽으면 응답을 기다리던 턴을 닫아 스피너가 영원히 남지
            // 않게 한다.
            reader_active.lock().await.active.clear();
            let mut detached = reader_detached.lock().await;
            for (_, turn) in detached.drain() {
                let _ = reader_events.send(ServerEvent::Notification {
                    method: "turn/completed".to_owned(),
                    params: json!({
                        "threadId": turn.session_id,
                        "turn": {
                            "id": turn.turn_id,
                            "error": { "message": tr("OpenCode ACP 연결이 종료되었습니다.", "The OpenCode ACP connection closed.") }
                        }
                    }),
                });
            }
            drop(detached);
            let cleared = {
                let mut streams = reader_streams.lock().await;
                streams
                    .iter_mut()
                    .filter_map(|(session_id, stream)| stream.all_subagents_cleared(session_id))
                    .collect::<Vec<_>>()
            };
            for (method, params) in cleared {
                notify(&reader_events, &method, params);
            }
            let _ = reader_events.send(ServerEvent::Closed(tr("OpenCode ACP 연결이 종료되었습니다.", "The OpenCode ACP connection closed.").to_owned()));
        });

        let stderr_events = event_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("ERROR") || line.contains("Error") {
                    let _ = stderr_events.send(ServerEvent::ProtocolWarning(line));
                }
            }
        });

        let subagent_auth = provider_auth.clone();
        let subagent_streams = Arc::clone(&streams);
        let subagent_events = event_tx.clone();
        let subagent_task = tokio::spawn(async move {
            // 2.x reports a child's end over ACP instead; see `route_message`.
            if subagent_auth.is_v2() {
                return;
            }
            loop {
                sleep(BACKGROUND_SUBAGENT_STATUS_POLL).await;
                if subagent_events.is_closed() {
                    break;
                }
                let has_background = subagent_streams
                    .lock()
                    .await
                    .values()
                    .any(SessionStreams::has_background_subagents);
                if !has_background {
                    continue;
                }
                let Ok(statuses) = subagent_auth.session_status().await else {
                    continue;
                };
                if !statuses.is_object() {
                    continue;
                }
                let updates = {
                    let mut streams = subagent_streams.lock().await;
                    streams
                        .iter_mut()
                        .filter_map(|(session_id, stream)| {
                            stream.reconcile_background_subagents(session_id, &statuses)
                        })
                        .collect::<Vec<_>>()
                };
                for (method, params) in updates {
                    notify(&subagent_events, &method, params);
                }
            }
        });

        let client = OpenCodeClient {
            outbound,
            inbound,
            pending,
            detached,
            active_turns,
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: event_tx,
        };
        Ok(Self {
            child,
            _job: job,
            client,
            provider_auth,
            helper,
            _helper_job: helper_job,
            loading,
            history: Arc::new(Mutex::new(HashMap::new())),
            events,
            writer_task,
            reader_task,
            stderr_task,
            subagent_task,
        })
    }

    pub async fn initialize(&self) -> Result<Value> {
        let mut meta = json!({ "terminal-auth": true });
        if self.provider_auth.is_v2() {
            // Without this, 2.x streams subagent text into the parent's answer.
            meta["opencode/child-session-updates"] = Value::Bool(true);
        }
        self.client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false,
                        "elicitation": { "form": {} },
                        "_meta": meta
                    },
                    "clientInfo": {
                        "name": "devez-vibe",
                        "title": "Devez Vibe",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await
    }

    pub fn provider_api(&self) -> ProviderAuthServer {
        self.provider_auth.clone()
    }

    pub async fn model_catalog(&self, cwd: &Path) -> Result<Value> {
        if self.provider_auth.is_v2() {
            return self.provider_auth.v2_models().await;
        }
        let response = self
            .client
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .context(tr("OpenCode 모델 조회 세션에 id가 없습니다.", "The OpenCode model lookup session has no id."))?;
        let provider_catalog = self
            .provider_auth
            .model_catalog()
            .await
            .unwrap_or(Value::Null);
        let current_model = current_model(&response);
        let current_efforts = config_option_values(&response, "effort");
        let connected = connected_provider_ids();
        let models = model_options(&response)
            .into_iter()
            .filter_map(|(value, name)| {
                let provider = value.split_once('/')?.0;
                if !provider_visible(&connected, provider) {
                    return None;
                }
                let mut efforts = model_reasoning_efforts(&provider_catalog, &value);
                if efforts.is_empty() && current_model == Some(value.as_str()) {
                    efforts.clone_from(&current_efforts);
                }
                Some(open_code_model(&value, &name, &efforts))
            })
            .collect::<Vec<_>>();
        let _ = self
            .client
            .request("session/close", json!({ "sessionId": session_id }))
            .await;
        Ok(json!({ "data": models }))
    }

    pub async fn provider_catalog(&self) -> Result<Value> {
        self.provider_auth.catalog().await
    }

    /// 2.x keeps credentials in its database, so `auth.json` may be stale or missing.
    pub async fn provider_connected(&self) -> bool {
        if !self.provider_auth.is_v2() {
            return has_connected_provider();
        }
        self.provider_auth.catalog().await.is_ok_and(|catalog| {
            catalog["connected"]
                .as_array()
                .is_some_and(|connected| !connected.is_empty())
        })
    }

    pub async fn set_provider_api_key(
        &self,
        provider_id: &str,
        key: &str,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        self.provider_auth
            .set_api_key(provider_id, key, inputs)
            .await
    }

    pub async fn authorize_provider_oauth(
        &self,
        provider_id: &str,
        method: usize,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        self.provider_auth
            .oauth_authorize(provider_id, method, inputs)
            .await
    }

    pub async fn complete_provider_oauth(
        &self,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<()> {
        self.provider_auth
            .oauth_callback(provider_id, method, code)
            .await
    }

    pub async fn start_session(
        &self,
        cwd: &Path,
        model: &str,
        effort: Option<&str>,
    ) -> Result<Value> {
        let response = self
            .client
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .context(tr("OpenCode 새 세션에 id가 없습니다.", "The new OpenCode session has no id."))?;
        let model = strip_model_prefix(model);
        self.set_model(session_id, model, effort).await?;
        Ok(thread_response(
            session_id,
            cwd,
            &format!("opencode:{model}"),
            effort.unwrap_or("default"),
        ))
    }

    pub async fn resume_session(&self, cwd: &Path, session_id: &str) -> Result<Value> {
        // session/load는 응답 전에 과거 대화를 session/update로 재생한다.
        // 그 조각을 화면에 흘리는 대신 모아서 재개 화면을 채운다.
        self.loading
            .lock()
            .await
            .insert(session_id.to_owned(), LoadedHistory::default());
        let result = self
            .client
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": []
                }),
            )
            .await;
        let loaded = self
            .loading
            .lock()
            .await
            .remove(session_id)
            .unwrap_or_default();
        let response = result?;
        let turns = history_turns(session_id, &loaded.chunks, loaded.plan.as_deref());
        let model = current_model(&response)
            .map(|model| format!("opencode:{model}"))
            .unwrap_or_else(|| "opencode:unknown/unknown".to_owned());
        let effort = current_config_option(&response, "effort").unwrap_or("default");
        let mut value = thread_response(session_id, cwd, &model, effort);
        value["thread"]["turns"] = Value::Array(turns.clone());
        if let Some(token_usage) = loaded.token_usage {
            value["tokenUsage"] = token_usage;
        }
        self.history.lock().await.insert(
            session_id.to_owned(),
            json!({ "data": turns, "nextCursor": null }),
        );
        Ok(value)
    }

    /// 마지막 session/load가 재생해 준 과거 대화 페이지. thread/turns/list가
    /// 사이드카 없이도 재개 내역을 채울 수 있게 한다.
    pub async fn history_page(&self, session_id: &str) -> Option<Value> {
        self.history.lock().await.get(session_id).cloned()
    }

    pub async fn list_sessions(&self, cwd: Option<&Path>) -> Result<Value> {
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        let response = self.client.request("session/list", params).await?;
        let roots = if self.provider_auth.is_v2() {
            self.provider_auth.v2_root_sessions(cwd).await.ok()
        } else {
            None
        };
        let data = response
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|session| {
                roots.as_ref().is_none_or(|roots| {
                    let id = session.get("sessionId").and_then(Value::as_str);
                    roots.iter().any(|root| Some(root.as_str()) == id)
                })
            })
            .map(|session| {
                json!({
                    "id": session.get("sessionId").cloned().unwrap_or(Value::Null),
                    "name": session.get("title").cloned().unwrap_or(Value::Null),
                    "cwd": session.get("cwd").cloned().unwrap_or(Value::Null),
                    "updatedAt": session.get("updatedAt").cloned().unwrap_or(Value::Null),
                    "source": "opencode"
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "data": data, "nextCursor": response.get("nextCursor") }))
    }

    pub async fn fork_session(
        &self,
        cwd: &Path,
        session_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<Value> {
        let response = self
            .client
            .request(
                "session/fork",
                json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": []
                }),
            )
            .await?;
        let forked = response
            .get("sessionId")
            .and_then(Value::as_str)
            .context(tr("OpenCode 분기 세션에 id가 없습니다.", "The forked OpenCode session has no id."))?
            .to_owned();
        // A fork starts on the workspace defaults, so the parent's own selection
        // has to be written onto the new session before it answers anything.
        if let Some(model) = model.filter(|model| is_open_code_model(model)) {
            self.set_model(&forked, model, effort).await?;
            return Ok(thread_response(
                &forked,
                cwd,
                model,
                effort.unwrap_or("default"),
            ));
        }
        let model = current_model(&response)
            .map(|model| format!("opencode:{model}"))
            .unwrap_or_else(|| "opencode:unknown/unknown".to_owned());
        let effort = current_config_option(&response, "effort").unwrap_or("default");
        Ok(thread_response(&forked, cwd, &model, effort))
    }

    pub async fn start_prompt(&self, session_id: &str, text: &str) -> Result<String> {
        self.client
            .start_prompt(
                session_id,
                vec![json!({ "type": "text", "text": text })],
                false,
            )
            .await
    }

    pub async fn turn_status(&self, session_id: &str, expected_turn: Option<&str>) -> Value {
        self.client.turn_status(session_id, expected_turn).await
    }

    pub async fn start_prompt_content(
        &self,
        session_id: &str,
        input: &[Value],
        instructions: Option<&str>,
        steer: bool,
    ) -> Result<String> {
        let mut prompt = Vec::new();
        if let Some(instructions) = instructions {
            prompt.push(json!({
                "type": "text",
                "text": format!(
                    "<devez-vibe-rules>\n{instructions}\n</devez-vibe-rules>"
                )
            }));
        }
        if let Some(instruction) = explicit_skill_instruction(input) {
            prompt.push(json!({ "type": "text", "text": instruction }));
        }
        for item in input {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        prompt.push(json!({ "type": "text", "text": text }));
                    }
                }
                Some("localImage") => {
                    let path = item
                        .get("path")
                        .and_then(Value::as_str)
                        .context(tr("이미지 입력에 경로가 없습니다.", "The image input has no path."))?;
                    let data = fs::read(path).with_context(|| {
                        tr(format!("이미지를 읽지 못했습니다: {path}"), format!("Couldn't read the image: {path}"))
                    })?;
                    prompt.push(json!({
                        "type": "image",
                        "data": BASE64.encode(data),
                        "mimeType": image_mime(Path::new(path))
                    }));
                }
                _ => {}
            }
        }
        if prompt.is_empty() {
            prompt.push(json!({ "type": "text", "text": "" }));
        }
        self.client.start_prompt(session_id, prompt, steer).await
    }

    pub async fn set_model(
        &self,
        session_id: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Result<()> {
        self.client
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": session_id,
                    "configId": "model",
                    "value": strip_model_prefix(model)
                }),
            )
            .await?;
        if let Some(effort) = effort.filter(|effort| *effort != "default") {
            self.client
                .request(
                    "session/set_config_option",
                    json!({
                        "sessionId": session_id,
                        "configId": "effort",
                        "value": effort
                    }),
                )
                .await?;
        }
        Ok(())
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        self.client.cancel(session_id)
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.client.respond(id, result)
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        self.client.respond_error(id, code, message)
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) {
        self.client.close();
        let _ = timeout(Duration::from_secs(2), &mut self.writer_task).await;
        if timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        self.reader_task.abort();
        self.stderr_task.abort();
        self.subagent_task.abort();
        if let Some(mut helper) = self.helper.take() {
            drop(helper.stdin.take());
            if timeout(Duration::from_secs(3), helper.wait()).await.is_err() {
                let _ = helper.kill().await;
            }
        }
    }
}

async fn route_message(
    message: Value,
    pending: &PendingMap,
    detached: &DetachedMap,
    active: &ActiveMap,
    loading: &LoadingMap,
    tools: &ToolMap,
    streams: &StreamMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
    inbound: &InboundMap,
    outbound: &Outbound,
) {
    if message.get("method").and_then(Value::as_str) == Some("$/cancel_request") {
        if let Some(id) = message.pointer("/params/requestId") {
            cancel_inbound_request(inbound, outbound, events, id);
        }
        return;
    }
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && (message.get("result").is_some() || message.get("error").is_some())
    {
        if let Some(sender) = pending.lock().await.remove(&id) {
            let response = if let Some(error) = message.get("error") {
                Err(format_rpc_error(error))
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(response);
            return;
        }
        let detached_turn = detached.lock().await.remove(&id);
        if let Some(turn) = detached_turn {
            let interrupted = message.pointer("/error/code").and_then(Value::as_i64)
                == Some(-32800)
                || message
                    .pointer("/result/stopReason")
                    .and_then(Value::as_str)
                    == Some("cancelled");
            let error_detail = message
                .get("error")
                .filter(|error| error.get("code").and_then(Value::as_i64) != Some(-32800))
                .map(format_rpc_error);
            if let Some(detail) = &error_detail {
                let _ = events.send(ServerEvent::Notification {
                    method: "error".to_owned(),
                    params: json!({
                        "threadId": turn.session_id,
                        "error": { "message": detail },
                        "willRetry": false,
                        "provider": "OpenCode"
                    }),
                });
            }
            // 합류한 추가 지시가 아직 돌고 있으면 턴을 끝내지 않는다.
            let finished = {
                let mut active = active.lock().await;
                match active.active.get_mut(&turn.session_id) {
                    Some(current) if current.turn_id == turn.turn_id => {
                        current.outstanding = current.outstanding.saturating_sub(1);
                        let done = current.outstanding == 0;
                        if done {
                            active.active.remove(&turn.session_id);
                        }
                        done
                    }
                    _ => true,
                }
            };
            if !finished {
                return;
            }
            cancel_inbound_requests(inbound, outbound, events, Some(&turn.session_id));
            if !detached.lock().await.values().any(|pending| {
                pending.session_id == turn.session_id && pending.turn_id == turn.turn_id
            }) {
                let mut turns = active.lock().await;
                turns.completed.push_back(turn.clone());
                while turns.completed.len() > COMPLETED_TURN_LIMIT {
                    turns.completed.pop_front();
                }
            }
            // 열린 본문·사고 항목은 턴이 끝나면 여기서 완료 처리해야
            // 진행 표시가 이 항목을 기다리며 남지 않는다.
            let (boundary, subagents) = {
                let mut streams = streams.lock().await;
                let entry = streams.entry(turn.session_id.clone()).or_default();
                (
                    entry.close_all(&turn.session_id),
                    entry.subagents_cleared(&turn.session_id),
                )
            };
            for (method, params) in boundary {
                notify(events, &method, params);
            }
            if let Some((method, params)) = subagents {
                notify(events, &method, params);
            }
            let mut completed = json!({ "id": turn.turn_id });
            if interrupted {
                completed["status"] = json!("interrupted");
            }
            if let Some(detail) = error_detail {
                completed["error"] = json!({ "message": detail });
            }
            notify(
                events,
                "turn/completed",
                json!({ "threadId": turn.session_id, "turn": completed }),
            );
            return;
        }
    }

    if message.get("method").and_then(Value::as_str) == Some("session/update") {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(child) = params
            .pointer("/update/_meta/opencode~1child-session/id")
            .and_then(Value::as_str)
            && child != session_id
        {
            route_child_update(
                &session_id,
                child,
                params.get("update"),
                false,
                tools,
                streams,
                events,
            )
            .await;
            return;
        }
        // session/load가 재생 중인 세션의 조각은 화면 대신 복원 버퍼로 간다.
        {
            let mut loading = loading.lock().await;
            if let Some(loaded) = loading.get_mut(&session_id) {
                if let Some(update) = params.get("update") {
                    if let Some(usage) = usage_update_token_usage(update) {
                        loaded.token_usage = Some(usage);
                    } else if let Some(plan) = plan_update(update) {
                        loaded.plan = Some(plan);
                    } else {
                        record_history_chunk(&mut loaded.chunks, update);
                    }
                }
                return;
            }
        }
        route_session_update(params, tools, streams, events).await;
        return;
    }

    // 자식 본문은 부모 답변으로 보내지 않고 부모 도구 행의 기록으로 모은다.
    if message.get("method").and_then(Value::as_str) == Some("opencode/session/child_update") {
        let params = message.get("params").unwrap_or(&Value::Null);
        if let (Some(root), Some(child)) = (
            params.get("rootSessionId").and_then(Value::as_str),
            params.get("childSessionId").and_then(Value::as_str),
        ) {
            let ended = matches!(
                params.get("status").and_then(Value::as_str),
                Some("completed" | "failed" | "interrupted")
            );
            route_child_update(
                root,
                child,
                params.get("update"),
                ended,
                tools,
                streams,
                events,
            )
            .await;
        }
        return;
    }

    if let (Some(id), Some(method)) = (
        message.get("id").cloned(),
        message.get("method").and_then(Value::as_str),
    ) {
        let (kind, method, mut params) = match method {
            "session/request_permission" => {
                let (method, params) = permission_request_params(
                    message.get("params").cloned().unwrap_or(Value::Null),
                );
                ("permission", method, params)
            }
            "elicitation/create" => {
                let params = message.get("params").unwrap_or(&Value::Null);
                (
                    "elicitation",
                    "mcpServer/elicitation/request".to_owned(),
                    json!({
                        "itemId": params.get("toolCallId"),
                        "serverName": "OpenCode",
                        "mode": params.get("mode"),
                        "message": params.get("message"),
                        "requestedSchema": params.get("requestedSchema")
                    }),
                )
            }
            _ => return,
        };
        let session_id = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        params["threadId"] = json!(session_id);
        let wrapped = json!({ "backend": "opencode", "kind": kind, "id": id });
        inbound.lock().expect("inbound mutex").insert(
            id.to_string(),
            InboundRequest {
                id: wrapped.clone(),
                session_id: session_id.to_owned(),
            },
        );
        let _ = events.send(ServerEvent::Request {
            id: wrapped,
            method,
            params,
        });
    }
}

fn cancel_inbound_request(
    inbound: &InboundMap,
    outbound: &Outbound,
    events: &mpsc::UnboundedSender<ServerEvent>,
    id: &Value,
) {
    let Some(request) = inbound
        .lock()
        .expect("inbound mutex")
        .remove(&id.to_string())
    else {
        return;
    };
    notify(
        events,
        "serverRequest/resolved",
        json!({
            "threadId": request.session_id, "requestId": request.id,
        }),
    );
    if let Some(sender) = outbound.lock().expect("outbound mutex").as_ref() {
        let _ = sender.send(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32800, "message": "Request cancelled" }
        }));
    }
}

fn cancel_inbound_requests(
    inbound: &InboundMap,
    outbound: &Outbound,
    events: &mpsc::UnboundedSender<ServerEvent>,
    session_id: Option<&str>,
) {
    let ids: Vec<Value> = inbound
        .lock()
        .expect("inbound mutex")
        .values()
        .filter(|request| session_id.is_none_or(|session_id| request.session_id == session_id))
        .map(|request| request.id["id"].clone())
        .collect();
    for id in ids {
        cancel_inbound_request(inbound, outbound, events, &id);
    }
}

async fn route_child_update(
    root: &str,
    child: &str,
    update: Option<&Value>,
    ended: bool,
    tools: &ToolMap,
    streams: &StreamMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
    if root.is_empty() || child.is_empty() || root == child {
        return;
    }
    let mut tools = tools.lock().await;
    let mut streams = streams.lock().await;
    if let Some(stream) = streams.get(child) {
        if stream
            .child_root
            .as_deref()
            .is_some_and(|parent| parent != root)
        {
            return;
        }
        if !ended && stream.child_completed_at.is_some() {
            streams.remove(child);
        }
    }
    let stream = streams.entry(child.to_owned()).or_default();
    stream.child_root = Some(root.to_owned());
    if let Some(update) = update {
        stream.record_child_update(child, update, &mut tools);
    }
    if ended {
        let boundary = stream.close_all(child);
        stream.record_child_boundary(boundary);
        stream.child_completed_at = Some(Instant::now());
        let prefix = format!("{child}:");
        tools.retain(|id, _| !id.starts_with(&prefix));
    }
    let tool_id = streams
        .get(root)
        .and_then(|root| root.child_tool_ids.get(child))
        .cloned();
    let history = tool_id
        .as_ref()
        .and_then(|id| streams.get(child)?.child_history(root, id));
    if let Some((method, params)) = history {
        notify(events, &method, params);
    }
    if ended {
        // 재개 자식은 부모 결과보다 먼저 끝날 수 있다. 본문 대신 종료 표식만 남긴다.
        if let Some(tool) = tool_id.as_ref().and_then(|id| tools.get_mut(id)) {
            tool["_devezChildCompleted"] = Value::Bool(true);
        }
        if let Some(stream) = streams.get_mut(root) {
            stream.child_tool_ids.remove(child);
            if let Some((method, params)) = stream.background_subagent_ended(root, child) {
                notify(events, &method, params);
            }
        }
        if tool_id.is_some() {
            streams.remove(child);
        } else {
            prune_completed_children(&mut streams);
        }
    }
}

fn prune_completed_children(streams: &mut HashMap<String, SessionStreams>) {
    let mut completed = streams
        .iter()
        .filter_map(|(id, stream)| {
            let at = stream.child_completed_at?;
            let bytes = serde_json::to_vec(&stream.child_items).map_or(0, |items| items.len())
                + id.len()
                + stream.child_root.as_ref().map_or(0, String::len);
            Some((id.clone(), at, bytes))
        })
        .collect::<Vec<_>>();
    completed.sort_by_key(|(_, at, _)| *at);
    let mut count = completed.len();
    let mut bytes = completed.iter().map(|(_, _, bytes)| bytes).sum::<usize>();
    for (id, _, size) in completed {
        if count <= COMPLETED_CHILD_LIMIT && bytes <= COMPLETED_CHILD_BYTES {
            break;
        }
        streams.remove(&id);
        count -= 1;
        bytes = bytes.saturating_sub(size);
    }
}

async fn link_child_session(
    session_id: &str,
    update: &Value,
    streams: &StreamMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) -> bool {
    if update.get("_devezChildCompleted").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if update.get("name").and_then(Value::as_str) != Some("subagent") {
        return false;
    }
    let metadata = update.pointer("/rawOutput/metadata/sessionID");
    let Some(child) = metadata
        .or_else(|| update.pointer("/rawInput/sessionID"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && *id != session_id)
    else {
        return false;
    };
    let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
        return false;
    };
    let mut streams = streams.lock().await;
    if streams
        .get(child)
        .and_then(|stream| stream.child_root.as_deref())
        .is_some_and(|root| root != session_id)
    {
        return false;
    }
    let completed = streams
        .get(child)
        .is_some_and(|stream| stream.child_completed_at.is_some())
        || update
            .pointer("/rawOutput/metadata/status")
            .and_then(Value::as_str)
            == Some("completed");
    if completed && metadata.is_none() {
        streams.remove(child);
    }
    if let Some((method, params)) = streams
        .get(child)
        .and_then(|stream| stream.child_history(session_id, id))
    {
        notify(events, &method, params);
    }
    let root = streams.entry(session_id.to_owned()).or_default();
    if completed && metadata.is_some() {
        root.child_tool_ids.remove(child);
        streams.remove(child);
        true
    } else {
        root.child_tool_ids.insert(child.to_owned(), id.to_owned());
        false
    }
}

async fn route_session_update(
    params: Value,
    tools: &ToolMap,
    streams: &StreamMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(update) = params.get("update") else {
        return;
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            if let Some(delta) =
                content_text(update.get("content")).filter(|delta| !delta.is_empty())
            {
                let message_id = update.get("messageId").and_then(Value::as_str);
                let notifications = streams
                    .lock()
                    .await
                    .entry(session_id.to_owned())
                    .or_default()
                    .message_chunk(session_id, message_id, delta);
                for (method, params) in notifications {
                    notify(events, &method, params);
                }
            }
        }
        Some("agent_thought_chunk") => {
            let notifications = streams
                .lock()
                .await
                .entry(session_id.to_owned())
                .or_default()
                .thought_boundary(session_id);
            for (method, params) in notifications {
                notify(events, &method, params);
            }
        }
        Some("tool_call") => {
            // 도구 호출이 시작되면 그 앞의 본문은 진행 문장으로 확정된다.
            let boundary = streams
                .lock()
                .await
                .entry(session_id.to_owned())
                .or_default()
                .close_all(session_id);
            for (method, params) in boundary {
                notify(events, &method, params);
            }
            let item = tool_item(update, false);
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                tools.lock().await.insert(id.to_owned(), update.clone());
            }
            link_child_session(session_id, update, streams, events).await;
            emit_plan(update, session_id, events);
            notify(
                events,
                "item/started",
                json!({ "threadId": session_id, "item": item }),
            );
            if let Some((method, params)) = streams
                .lock()
                .await
                .entry(session_id.to_owned())
                .or_default()
                .subagent_started(session_id, update)
            {
                notify(events, &method, params);
            }
        }
        Some("tool_call_update") => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("opencode-tool");
            let previous = tools.lock().await.get(id).cloned();
            let merged = merge_tool_update(previous, update);
            let child_completed = link_child_session(session_id, &merged, streams, events).await;
            emit_plan(&merged, session_id, events);
            match update.get("status").and_then(Value::as_str) {
                status @ Some("completed" | "failed") => {
                    tools.lock().await.remove(id);
                    notify(
                        events,
                        "item/completed",
                        json!({ "threadId": session_id, "item": tool_item(&merged, true) }),
                    );
                    let subagents = {
                        let mut streams = streams.lock().await;
                        let entry = streams.entry(session_id.to_owned()).or_default();
                        if status == Some("completed") && !child_completed {
                            entry.subagent_tool_completed(session_id, id, &merged)
                        } else {
                            entry.subagent_finished(session_id, id)
                        }
                    };
                    if let Some((method, params)) = subagents {
                        notify(events, &method, params);
                    }
                }
                _ => {
                    let described = streams
                        .lock()
                        .await
                        .entry(session_id.to_owned())
                        .or_default()
                        .subagent_described(session_id, &merged);
                    tools.lock().await.insert(id.to_owned(), merged);
                    if let Some((method, params)) = described {
                        notify(events, &method, params);
                    }
                }
            }
        }
        Some("plan") => {
            emit_plan(update, session_id, events);
        }
        Some("usage_update") => {
            let Some(token_usage) = usage_update_token_usage(update) else {
                return;
            };
            notify(
                events,
                "thread/tokenUsage/updated",
                json!({
                    "threadId": session_id,
                    "tokenUsage": token_usage
                }),
            );
        }
        Some("session_info_update" | "compaction_update") => {
            let compaction = if update.get("sessionUpdate").and_then(Value::as_str)
                == Some("compaction_update")
            {
                update
            } else {
                &update["_meta"]["opencode/compaction"]
            };
            match compaction.get("status").and_then(Value::as_str) {
                Some("started") => notify(
                    events,
                    "rawResponseItem/completed",
                    json!({ "threadId": session_id, "item": { "type": "compaction_trigger" } }),
                ),
                Some("completed") => notify(
                    events,
                    "thread/compacted",
                    json!({ "threadId": session_id }),
                ),
                Some("failed") => notify(
                    events,
                    "thread/compaction/failed",
                    json!({ "threadId": session_id, "error": compaction.get("error").cloned().unwrap_or_else(|| json!({
                        "message": tr("컨텍스트 압축에 실패했습니다.", "Context compaction failed.")
                    })) }),
                ),
                _ => {}
            }
        }
        _ => {}
    }
}

fn usage_update_token_usage(update: &Value) -> Option<Value> {
    (update.get("sessionUpdate").and_then(Value::as_str) == Some("usage_update")).then(|| {
        let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
        json!({
            "last": { "totalTokens": used },
            "total": { "totalTokens": used },
            "modelContextWindow": update.get("size").cloned().unwrap_or(Value::Null)
        })
    })
}

fn tool_item(update: &Value, completed: bool) -> Value {
    let id = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("opencode-tool");
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other");
    let input = update.get("rawInput").cloned().unwrap_or_else(|| json!({}));
    if kind == "execute" {
        let command = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(Value::as_str)
            .unwrap_or(title);
        return json!({
            "id": id,
            "type": "commandExecution",
            "command": command,
            "status": if completed && update.get("status").and_then(Value::as_str) == Some("failed") {
                "failed"
            } else if completed {
                "completed"
            } else {
                "inProgress"
            },
            "aggregatedOutput": tool_output(update),
            "exitCode": tool_exit_code(update)
        });
    }
    // websearch 도구는 시작 때 도구 이름을, 실행 뒤에는 `<검색 제공자> "<검색어>"`를 제목으로 보낸다.
    let query = input.get("query").and_then(Value::as_str).unwrap_or_default();
    if title == "websearch" || (!query.is_empty() && title.ends_with(&format!("\"{query}\""))) {
        return json!({ "id": id, "type": "webSearch", "query": query });
    }
    if completed {
        let changes = tool_diffs(update);
        if !changes.is_empty() {
            return json!({ "id": id, "type": "fileChange", "changes": changes });
        }
    }
    json!({
        "id": id,
        "type": "dynamicToolCall",
        "tool": title,
        "arguments": input,
        "contentItems": if completed {
            json!([{ "type": "text", "text": tool_output(update) }])
        } else {
            json!([])
        }
    })
}

fn merge_tool_update(previous: Option<Value>, update: &Value) -> Value {
    let mut merged = previous
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(current) = update.as_object() {
        for (key, value) in current {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

fn emit_plan(update: &Value, session_id: &str, events: &mpsc::UnboundedSender<ServerEvent>) {
    let Some(plan) = plan_update(update) else {
        return;
    };
    notify(
        events,
        "turn/plan/updated",
        json!({ "threadId": session_id, "plan": plan }),
    );
}

fn plan_update(update: &Value) -> Option<Vec<Value>> {
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_plan_input = update
        .get("rawInput")
        .is_some_and(|input| input.get("todos").is_some() || input.get("plan").is_some());
    let native_plan = update.get("sessionUpdate").and_then(Value::as_str) == Some("plan");
    if !native_plan
        && !has_plan_input
        && title != "todowrite"
        && title != "update_plan"
        && title != "update plan"
    {
        return None;
    }
    let todos = if native_plan {
        update.get("entries")
    } else {
        update
            .get("rawInput")
            .and_then(|input| input.get("todos").or_else(|| input.get("plan")))
    }?
    .as_array()?;
    let plan = todos
        .iter()
        .filter_map(|todo| {
            let text = todo
                .get("content")
                .or_else(|| todo.get("step"))
                .or_else(|| todo.get("text"))
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())?;
            Some(json!({
                "id": todo.get("id").and_then(Value::as_str),
                "step": text,
                "status": plan_status(todo.get("status").and_then(Value::as_str)),
                "elapsedMs": todo.get("elapsedMs").and_then(Value::as_u64)
            }))
        })
        .collect::<Vec<_>>();
    Some(plan)
}

fn plan_status(status: Option<&str>) -> &'static str {
    match status {
        Some("completed" | "done") => "completed",
        Some("in_progress" | "inProgress" | "active") => "inProgress",
        _ => "pending",
    }
}

fn tool_output(update: &Value) -> String {
    if let Some(output) = update
        .get("rawOutput")
        .and_then(|raw| raw.get("output").or_else(|| raw.get("error")))
        .and_then(Value::as_str)
    {
        return output.to_owned();
    }
    update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_exit_code(update: &Value) -> Option<i64> {
    update
        .pointer("/rawOutput/metadata/exit")
        .or_else(|| update.pointer("/rawOutput/metadata/exitCode"))
        .and_then(Value::as_i64)
}

fn tool_diffs(update: &Value) -> Vec<Value> {
    update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("diff"))
        .map(|item| {
            let old = item
                .get("oldText")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let new = item
                .get("newText")
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!({
                "path": item.get("path").cloned().unwrap_or_else(|| json!("unknown")),
                "kind": { "type": if old.is_empty() { "add" } else { "update" } },
                "diff": simple_diff(old, new)
            })
        })
        .collect()
}

fn simple_diff(old: &str, new: &str) -> String {
    let old_lines = old.lines().count().max(1);
    let new_lines = new.lines().count().max(1);
    let mut rows = vec![format!("@@ -1,{old_lines} +1,{new_lines} @@")];
    rows.extend(old.lines().map(|line| format!("-{line}")));
    rows.extend(new.lines().map(|line| format!("+{line}")));
    rows.join("\n")
}

fn permission_request_params(params: Value) -> (String, Value) {
    let tool = params.get("toolCall").cloned().unwrap_or_else(|| json!({}));
    let kind = tool.get("kind").and_then(Value::as_str).unwrap_or("other");
    let raw = tool.get("rawInput").cloned().unwrap_or_else(|| json!({}));
    let method = match kind {
        "execute" => "item/commandExecution/requestApproval",
        "edit" | "delete" | "move" => "item/fileChange/requestApproval",
        _ => "item/permissions/requestApproval",
    }
    .to_owned();
    let request = match kind {
        "execute" => json!({
            "command": raw.get("command")
                .or_else(|| raw.get("cmd"))
                .cloned()
                .unwrap_or_else(|| tool.get("title").cloned().unwrap_or(Value::Null)),
            "cwd": raw.get("cwd").or_else(|| raw.get("workdir")).cloned(),
            "reason": tool.get("title").cloned()
        }),
        "edit" | "delete" | "move" => json!({
            "reason": tool.get("title").cloned(),
            "grantRoot": tool.pointer("/locations/0/path").cloned(),
            "opencodeOptions": params.get("options").cloned()
        }),
        _ => json!({
            "permissions": {
                "opencode": {
                    "tool": tool,
                    "options": params.get("options").cloned()
                }
            }
        }),
    };
    (method, request)
}

fn permission_result(result: &Value) -> Value {
    match result.get("decision").and_then(Value::as_str) {
        Some("accept") => json!({ "outcome": { "outcome": "selected", "optionId": "once" } }),
        Some("acceptForSession") => {
            json!({ "outcome": { "outcome": "selected", "optionId": "always" } })
        }
        _ if result.get("scope").and_then(Value::as_str) == Some("session") => {
            json!({ "outcome": { "outcome": "selected", "optionId": "always" } })
        }
        _ if result
            .get("permissions")
            .and_then(Value::as_object)
            .is_some_and(|permissions| !permissions.is_empty()) =>
        {
            json!({ "outcome": { "outcome": "selected", "optionId": "once" } })
        }
        _ => json!({ "outcome": { "outcome": "selected", "optionId": "reject" } }),
    }
}

fn open_code_request_id(id: &Value) -> Result<Value> {
    id.get("backend")
        .and_then(Value::as_str)
        .filter(|backend| *backend == "opencode")
        .and_then(|_| id.get("id"))
        .cloned()
        .context(tr("OpenCode 요청 id가 올바르지 않습니다.", "The OpenCode request id is invalid."))
}

pub fn is_open_code_request_id(id: &Value) -> bool {
    id.get("backend").and_then(Value::as_str) == Some("opencode")
}

fn notify(events: &mpsc::UnboundedSender<ServerEvent>, method: &str, params: Value) {
    let _ = events.send(ServerEvent::Notification {
        method: method.to_owned(),
        params,
    });
}

fn content_text(content: Option<&Value>) -> Option<&str> {
    content
        .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
}

fn model_options(response: &Value) -> Vec<(String, String)> {
    response
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("model"))
        .and_then(|option| option.get("options"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some((
                option.get("value")?.as_str()?.to_owned(),
                option.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn current_model(response: &Value) -> Option<&str> {
    current_config_option(response, "model")
}

fn current_config_option<'a>(response: &'a Value, id: &str) -> Option<&'a str> {
    response
        .get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some(id))?
        .get("currentValue")?
        .as_str()
}

fn config_option_values(response: &Value, id: &str) -> Vec<String> {
    response
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|option| option.get("options"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("value")?.as_str().map(ToOwned::to_owned))
        .collect()
}

fn model_reasoning_efforts(catalog: &Value, model: &str) -> Vec<String> {
    let Some((provider_id, model_id)) = model.split_once('/') else {
        return Vec::new();
    };
    let Some(variants) = catalog
        .get("all")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id))
        .and_then(|provider| provider.get("models"))
        .and_then(|models| models.get(model_id))
        .and_then(|model| model.get("variants"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut efforts = variants
        .iter()
        .filter_map(|(name, variant)| {
            variant
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .map(|effort| effort.to_owned())
                .or_else(|| EFFORT_ORDER.contains(&name.as_str()).then(|| name.clone()))
        })
        .collect::<Vec<_>>();
    efforts.sort_by_key(|effort| effort_rank(effort));
    efforts.dedup();
    efforts
}

const EFFORT_ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn effort_rank(effort: &str) -> usize {
    EFFORT_ORDER
        .iter()
        .position(|candidate| *candidate == effort)
        .unwrap_or(EFFORT_ORDER.len())
}

fn open_code_model(model: &str, display_name: &str, efforts: &[String]) -> Value {
    let default_effort = efforts.first().map(String::as_str).unwrap_or("default");
    let supported_efforts = efforts
        .iter()
        .map(|effort| json!({ "reasoningEffort": effort }))
        .collect::<Vec<_>>();
    json!({
        "id": format!("opencode:{model}"),
        "model": format!("opencode:{model}"),
        "displayName": open_code_model_display_name(model, display_name),
        "defaultReasoningEffort": default_effort,
        "supportedReasoningEfforts": supported_efforts,
        "isDefault": false
    })
}

fn open_code_model_display_name(model: &str, display_name: &str) -> String {
    let provider = model
        .split_once('/')
        .map(|(provider, _)| provider)
        .unwrap_or(model);
    let family = match provider {
        "opencode-go" => "OpenCode Go",
        "opencode" => "OpenCode Zen",
        _ => display_name
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or(provider),
    };
    let name = display_name
        .split_once('/')
        .map(|(_, name)| name)
        .unwrap_or(display_name);
    format!("{name} · {family}")
}

/// session/load 재생에서 온 본문 조각 하나를 누적한다. 같은 메시지의 조각은
/// 한 항목으로 합치고, Devez Vibe가 주입한 규칙 블록은 사용자 글이 아니므로
/// 버린다.
fn record_history_chunk(chunks: &mut Vec<HistoryChunk>, update: &Value) {
    let user = match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("user_message_chunk") => true,
        Some("agent_message_chunk") => false,
        _ => return,
    };
    let Some(text) = content_text(update.get("content")).filter(|text| !text.is_empty()) else {
        return;
    };
    if user && text.trim_start().starts_with("<devez-vibe-rules>") {
        return;
    }
    let message_id = update.get("messageId").and_then(Value::as_str);
    if let Some(last) = chunks.last_mut()
        && last.user == user
        && (message_id.is_none() || last.message_id.as_deref() == message_id)
    {
        last.text.push_str(text);
        return;
    }
    chunks.push(HistoryChunk {
        user,
        message_id: message_id.map(ToOwned::to_owned),
        text: text.to_owned(),
    });
}

/// 모은 조각을 load_history가 아는 turns 형태로 바꾼다. 사용자 메시지가 새
/// 턴을 열고 나머지 항목은 그 턴에 붙는다.
fn history_turns(session_id: &str, chunks: &[HistoryChunk], plan: Option<&[Value]>) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let id = format!("{session_id}-history-{index}");
        let item = if chunk.user {
            json!({
                "id": id,
                "type": "userMessage",
                "content": [{ "type": "text", "text": chunk.text }]
            })
        } else {
            json!({
                "id": id,
                "type": "agentMessage",
                "provider": "OpenCode",
                "text": chunk.text
            })
        };
        if chunk.user || turns.is_empty() {
            turns.push(json!({
                "id": format!("{session_id}-history-turn-{index}"),
                "status": "completed",
                "items": [item]
            }));
        } else if let Some(items) = turns
            .last_mut()
            .and_then(|turn| turn.get_mut("items"))
            .and_then(Value::as_array_mut)
        {
            items.push(item);
        }
    }
    if let Some(plan) = plan {
        if turns.is_empty() {
            turns.push(json!({ "id": format!("{session_id}-history-plan"), "items": [] }));
        }
        turns.last_mut().unwrap()["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id": format!("{session_id}-plan"), "type": "plan", "steps": plan
            }));
    }
    turns
}

fn thread_response(session_id: &str, cwd: &Path, model: &str, effort: &str) -> Value {
    json!({
        "id": session_id,
        "thread": {
            "id": session_id,
            "cwd": cwd,
            "turns": []
        },
        "cwd": cwd,
        "model": model,
        "reasoningEffort": effort
    })
}

fn strip_model_prefix(model: &str) -> &str {
    model.strip_prefix("opencode:").unwrap_or(model)
}

pub fn is_open_code_model(model: &str) -> bool {
    model.starts_with("opencode:")
}

pub fn has_connected_provider() -> bool {
    !connected_provider_ids().is_empty()
}

/// Zen (`opencode`) and Go (`opencode-go`) are one runtime from the picker's
/// point of view: authenticating either unlocks both model families. `openai`
/// stays hidden, and everything else still needs its own key.
fn provider_visible(connected: &[String], provider: &str) -> bool {
    if provider == "openai" {
        return false;
    }
    connected.is_empty()
        || connected.iter().any(|id| id == provider)
        || match provider {
            "opencode" | "opencode-go" => connected
                .iter()
                .any(|id| *id == "opencode" || *id == "opencode-go"),
            _ => false,
        }
}

fn connected_provider_ids() -> Vec<String> {
    let Some(path) = auth_file() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Map<String, Value>>(&content)
        .map(|providers| providers.into_iter().map(|(id, _)| id).collect())
        .unwrap_or_default()
}

fn auth_file() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(root).join("opencode").join("auth.json"));
    }
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|home| {
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json")
        })
}

fn format_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(tr("알 수 없는 OpenCode ACP 오류", "Unknown OpenCode ACP error"));
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} ({code})"),
        None => message.to_owned(),
    }
}

fn command_for(path: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let shell = env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
            let mut command = Command::new(shell);
            command.args(["/d", "/s", "/c"]).arg(path);
            return command;
        }
    }
    Command::new(path)
}

fn resolve_command(command: &Path) -> PathBuf {
    if command.components().count() > 1 || command.exists() {
        return command.to_path_buf();
    }
    let Some(path) = env::var_os("PATH") else {
        return command.to_path_buf();
    };
    #[cfg(windows)]
    let extensions = [".exe", ".cmd", ".bat", ".com", ".ps1"];
    #[cfg(not(windows))]
    let extensions = [""];
    for directory in env::split_paths(&path) {
        for extension in extensions {
            let candidate = directory.join(format!("{}{extension}", command.to_string_lossy()));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    command.to_path_buf()
}

fn image_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> (
        OpenCodeClient,
        mpsc::UnboundedReceiver<Value>,
        mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let (outbound, replies) = mpsc::unbounded_channel();
        let (events, requests) = mpsc::unbounded_channel();
        (
            OpenCodeClient {
                outbound: Arc::new(StdMutex::new(Some(outbound))),
                inbound: Arc::default(),
                pending: Arc::default(),
                detached: Arc::default(),
                active_turns: Arc::default(),
                next_id: Arc::new(AtomicU64::new(1)),
                next_turn: Arc::new(AtomicU64::new(1)),
                events,
            },
            replies,
            requests,
        )
    }

    async fn route_client_message(client: &OpenCodeClient, message: Value) {
        route_message(
            message,
            &client.pending,
            &client.detached,
            &client.active_turns,
            &Arc::default(),
            &Arc::default(),
            &Arc::default(),
            &client.events,
            &client.inbound,
            &client.outbound,
        )
        .await;
    }

    fn test_form_request(id: Value, session_id: &str) -> Value {
        json!({ "id": id, "method": "elicitation/create", "params": {
            "sessionId": session_id, "mode": "form", "message": "이름", "requestedSchema": {
                "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"]
            }
        } })
    }

    #[tokio::test]
    async fn cancelled_server_requests_resolve_exact_ids_and_release_form_ui() {
        use crate::state::{Action, AppState};

        let (client, mut replies, mut events) = test_client();
        let mut btw = AppState::new(
            "ses_btw".into(),
            "cwd".into(),
            "시험".into(),
            Vec::new(),
            "opencode:test/test",
            None,
        );
        let mut main = AppState::new(
            "ses_main".into(),
            "cwd".into(),
            "시험".into(),
            Vec::new(),
            "opencode:test/test",
            None,
        );
        route_client_message(&client, test_form_request(json!(7), "ses_btw")).await;
        let ServerEvent::Request {
            id: form_id,
            method,
            params,
        } = events.recv().await.unwrap()
        else {
            panic!("질문 요청");
        };
        assert!(matches!(
            btw.begin_server_request(form_id.clone(), &method, &params),
            Action::None
        ));
        route_client_message(
            &client,
            json!({ "id": "7", "method": "session/request_permission", "params": {
            "sessionId": "ses_main", "toolCall": { "toolCallId": "call_1", "title": "실행" }
        } }),
        )
        .await;
        let ServerEvent::Request {
            id: permission_id,
            method,
            params,
        } = events.recv().await.unwrap()
        else {
            panic!("승인 요청");
        };
        assert_eq!(params["threadId"], "ses_main");
        assert!(matches!(
            main.begin_server_request(permission_id.clone(), &method, &params),
            Action::None
        ));
        assert!(main.awaiting_input() && btw.awaiting_input());

        route_client_message(
            &client,
            json!({ "method": "$/cancel_request", "params": { "requestId": 99 } }),
        )
        .await;
        assert!(events.try_recv().is_err() && replies.try_recv().is_err());
        route_client_message(
            &client,
            json!({ "method": "$/cancel_request", "params": { "requestId": 7 } }),
        )
        .await;
        let ServerEvent::Notification { method, params } = events.recv().await.unwrap() else {
            panic!("취소 알림");
        };
        assert_eq!(method, "serverRequest/resolved");
        assert_eq!(
            params,
            json!({ "threadId": "ses_btw", "requestId": form_id })
        );
        btw.handle_notification(&method, &params);
        assert!(!btw.awaiting_input());
        assert!(main.awaiting_input());
        let error = replies.recv().await.unwrap();
        assert_eq!(error["id"], 7);
        assert_eq!(error["error"]["code"], -32800);
        assert_eq!(client.inbound.lock().unwrap().len(), 1);

        route_client_message(
            &client,
            json!({ "method": "$/cancel_request", "params": { "requestId": 7 } }),
        )
        .await;
        assert!(events.try_recv().is_err() && replies.try_recv().is_err());
        route_client_message(
            &client,
            json!({ "method": "$/cancel_request", "params": { "requestId": "7" } }),
        )
        .await;
        let ServerEvent::Notification { method, params } = events.recv().await.unwrap() else {
            panic!("승인 취소 알림");
        };
        assert_eq!(
            params,
            json!({ "threadId": "ses_main", "requestId": permission_id })
        );
        main.handle_notification(&method, &params);
        assert!(!main.awaiting_input());
        assert_eq!(replies.recv().await.unwrap()["id"], "7");
        assert!(client.inbound.lock().unwrap().is_empty());

        route_client_message(&client, test_form_request(json!(8), "ses_btw")).await;
        let ServerEvent::Request { id, .. } = events.recv().await.unwrap() else {
            panic!("질문 요청");
        };
        client.respond_error(id, -32602, "잘못된 질문").unwrap();
        assert!(client.inbound.lock().unwrap().is_empty());
        assert_eq!(replies.recv().await.unwrap()["error"]["code"], -32602);
        route_client_message(&client, test_form_request(json!(9), "ses_btw")).await;
        let ServerEvent::Request { id, .. } = events.recv().await.unwrap() else {
            panic!("질문 요청");
        };
        client.respond(id, json!({ "action": "cancel" })).unwrap();
        assert!(client.inbound.lock().unwrap().is_empty());
        assert_eq!(replies.recv().await.unwrap()["result"]["action"], "cancel");
        route_client_message(&client, test_form_request(json!(10), "ses_btw")).await;
        events.recv().await.unwrap();
        client.close();
        assert!(client.inbound.lock().unwrap().is_empty());
        assert!(
            matches!(events.recv().await.unwrap(), ServerEvent::Notification { method, params }
            if method == "serverRequest/resolved" && params["threadId"] == "ses_btw")
        );
        assert_eq!(replies.recv().await.unwrap()["error"]["code"], -32800);
    }

    #[tokio::test]
    async fn turn_completion_cancels_only_its_unanswered_requests() {
        let (client, mut replies, mut events) = test_client();
        for (session_id, steer) in [("ses_1", false), ("ses_1", true), ("ses_2", false)] {
            client
                .start_prompt(session_id, Vec::new(), steer)
                .await
                .unwrap();
        }
        let first = replies.recv().await.unwrap()["id"].clone();
        let joined = replies.recv().await.unwrap()["id"].clone();
        let other = replies.recv().await.unwrap()["id"].clone();
        while events.try_recv().is_ok() {}
        for (id, session_id) in [("form_1", "ses_1"), ("form_2", "ses_2")] {
            route_client_message(&client, test_form_request(json!(id), session_id)).await;
            events.recv().await.unwrap();
        }
        route_client_message(&client, json!({ "id": first, "result": {} })).await;
        assert!(events.try_recv().is_err() && replies.try_recv().is_err());
        assert_eq!(client.inbound.lock().unwrap().len(), 2);
        route_client_message(
            &client,
            json!({ "id": joined, "error": { "code": -32800, "message": "Request cancelled" } }),
        )
        .await;
        let notifications = drain_notifications(&mut events);
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[0].0, "serverRequest/resolved");
        assert_eq!(notifications[0].1["requestId"]["id"], "form_1");
        assert_eq!(notifications[0].1["threadId"], "ses_1");
        assert_eq!(notifications[1].0, "turn/completed");
        assert_eq!(notifications[1].1["turn"]["status"], "interrupted");
        assert!(notifications[1].1["turn"].get("error").is_none());
        assert_eq!(replies.recv().await.unwrap()["id"], "form_1");
        assert_eq!(client.inbound.lock().unwrap().len(), 1);
        route_client_message(
            &client,
            json!({ "id": other, "result": { "stopReason": "cancelled" } }),
        )
        .await;
        let notifications = drain_notifications(&mut events);
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[1].1["turn"]["status"], "interrupted");
        assert_eq!(replies.recv().await.unwrap()["id"], "form_2");
        assert!(client.inbound.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn elicitation_requests_round_trip_through_existing_form_ui() {
        use crate::state::{Action, AppState};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (outbound, mut replies) = mpsc::unbounded_channel();
        let (events, mut requests) = mpsc::unbounded_channel();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            inbound: Arc::default(),
            pending: Arc::default(),
            detached: Arc::default(),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events,
        };
        for (key, action) in [
            (KeyEvent::from(KeyCode::Enter), "accept"),
            (KeyEvent::from(KeyCode::Esc), "cancel"),
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                "decline",
            ),
        ] {
            route_message(
                json!({ "id": "question_1", "method": "elicitation/create", "params": {
                    "mode": "form", "sessionId": "ses_1", "toolCallId": "call_1",
                    "message": "Which package manager?", "requestedSchema": {
                        "type": "object", "properties": { "pm": {
                            "type": "string", "oneOf": [{ "const": "bun", "title": "Bun" }]
                        } }, "required": ["pm"]
                    }
                } }),
                &client.pending,
                &client.detached,
                &client.active_turns,
                &Arc::default(),
                &Arc::default(),
                &Arc::default(),
                &client.events,
                &client.inbound,
                &client.outbound,
            )
            .await;
            let ServerEvent::Request { id, method, params } = requests.recv().await.unwrap() else {
                panic!("질문 폼 요청이어야 한다");
            };
            assert_eq!(id["kind"], "elicitation");
            assert_eq!(params["threadId"], "ses_1");
            assert_eq!(params["itemId"], "call_1");
            let mut state = AppState::new(
                "ses_1".into(),
                "cwd".into(),
                "시험".into(),
                Vec::new(),
                "opencode:test/test",
                None,
            );
            assert!(matches!(
                state.begin_server_request(id.clone(), &method, &params),
                Action::None
            ));
            let Action::RpcResponse {
                id: returned_id,
                result,
            } = state.handle_key(key)
            else {
                panic!("질문 폼 응답이어야 한다");
            };
            assert_eq!(returned_id, id);
            assert_eq!(result["action"], action);
            if action == "accept" {
                assert_eq!(result["content"], json!({ "pm": "bun" }));
            }
            client.respond(returned_id, result.clone()).unwrap();
            let reply = replies.recv().await.unwrap();
            assert_eq!(reply["id"], "question_1");
            assert_eq!(reply["result"], result);
            assert!(reply["result"].get("outcome").is_none());
        }
        client
            .respond_error(
                json!({ "backend": "opencode", "kind": "elicitation", "id": 9 }),
                -32602,
                "잘못된 질문",
            )
            .unwrap();
        assert_eq!(
            replies.recv().await.unwrap(),
            json!({
                "jsonrpc": "2.0", "id": 9,
                "error": { "code": -32602, "message": "잘못된 질문" }
            })
        );
    }

    #[tokio::test]
    async fn permission_requests_keep_acp_decisions_after_elicitation_support() {
        let (outbound, mut replies) = mpsc::unbounded_channel();
        let (events, mut requests) = mpsc::unbounded_channel();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            inbound: Arc::default(),
            pending: Arc::default(),
            detached: Arc::default(),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events,
        };
        route_message(
            json!({ "id": 12, "method": "session/request_permission", "params": {
                "sessionId": "ses_1", "toolCall": { "toolCallId": "call_1", "title": "실행" }
            } }),
            &client.pending,
            &client.detached,
            &client.active_turns,
            &Arc::default(),
            &Arc::default(),
            &Arc::default(),
            &client.events,
            &client.inbound,
            &client.outbound,
        )
        .await;
        let ServerEvent::Request { id, .. } = requests.recv().await.unwrap() else {
            panic!("승인 요청이어야 한다");
        };
        assert_eq!(id["kind"], "permission");
        for (result, option) in [
            (json!({ "decision": "accept" }), "once"),
            (json!({ "decision": "acceptForSession" }), "always"),
            (json!({ "decision": "decline" }), "reject"),
        ] {
            client.respond(id.clone(), result).unwrap();
            let reply = replies.recv().await.unwrap();
            assert_eq!(reply["id"], 12);
            assert_eq!(
                reply["result"],
                json!({
                    "outcome": { "outcome": "selected", "optionId": option }
                })
            );
        }
        client
            .respond(
                json!({ "backend": "opencode", "id": "legacy" }),
                json!({ "decision": "accept" }),
            )
            .unwrap();
        assert_eq!(
            replies.recv().await.unwrap()["result"]["outcome"]["optionId"],
            "once"
        );
    }

    #[tokio::test]
    async fn rename_uses_the_runtime_api_and_reports_failed_writes() {
        use tokio::io::AsyncReadExt;

        for v2 in [false, true] {
            for success in [true, false] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let mut api =
                    ProviderAuthServer::new(listener.local_addr().unwrap().port()).unwrap();
                if v2 {
                    api.password = Some("test-password".to_owned());
                }
                let server = tokio::spawn(async move {
                    for health in [true, false] {
                        let (stream, _) = listener.accept().await.unwrap();
                        let mut stream = BufReader::new(stream);
                        let mut line = String::new();
                        stream.read_line(&mut line).await.unwrap();
                        let path = if health {
                            if v2 { "/api/info" } else { "/global/health" }
                        } else if v2 {
                            "/api/session/ses_test"
                        } else {
                            "/session/ses_test"
                        };
                        let method = if health { "GET" } else { "PATCH" };
                        assert_eq!(line.trim(), format!("{method} {path} HTTP/1.1"));
                        let mut length = 0;
                        let mut authenticated = false;
                        loop {
                            line.clear();
                            stream.read_line(&mut line).await.unwrap();
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            let lower = line.to_ascii_lowercase();
                            if let Some(value) = lower.strip_prefix("content-length:") {
                                length = value.trim().parse::<usize>().unwrap();
                            }
                            authenticated |= lower.starts_with("authorization: basic ");
                        }
                        assert_eq!(authenticated, v2);
                        let mut body = vec![0; length];
                        stream.read_exact(&mut body).await.unwrap();
                        if !health {
                            assert_eq!(
                                serde_json::from_slice::<Value>(&body).unwrap(),
                                json!({ "title": "새 이름" })
                            );
                        }
                        let response = if health || success {
                            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned()
                        } else {
                            let body = r#"{"message":"rename denied"}"#;
                            format!(
                                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        stream
                            .get_mut()
                            .write_all(response.as_bytes())
                            .await
                            .unwrap();
                    }
                });
                let result = api.rename_session("ses_test", "새 이름").await;
                server.await.unwrap();
                if success {
                    assert!(result.unwrap().is_null());
                } else {
                    assert!(result.unwrap_err().to_string().contains("rename denied"));
                }
            }
        }
    }

    #[tokio::test]
    async fn compaction_updates_report_start_success_and_failure() {
        let tools: ToolMap = Arc::default();
        let streams: StreamMap = Arc::default();
        let (events, mut rx) = mpsc::unbounded_channel();
        for native in [false, true] {
            for (status, expected) in [
                ("started", "rawResponseItem/completed"),
                ("completed", "thread/compacted"),
                ("failed", "thread/compaction/failed"),
            ] {
                let marker = json!({ "status": status, "error": { "message": "provider failed" } });
                let update = if native {
                    let mut update = marker;
                    update["sessionUpdate"] = json!("compaction_update");
                    update
                } else {
                    json!({ "sessionUpdate": "session_info_update", "_meta": { "opencode/compaction": marker } })
                };
                route_session_update(
                    json!({ "sessionId": "ses_test", "update": update }),
                    &tools,
                    &streams,
                    &events,
                )
                .await;
                let notices = drain_notifications(&mut rx);
                assert_eq!(notices.len(), 1);
                assert_eq!(notices[0].0, expected);
                assert_eq!(notices[0].1["threadId"], "ses_test");
                if status == "failed" {
                    assert_eq!(notices[0].1["error"]["message"], "provider failed");
                }
            }
        }
        route_session_update(
            json!({ "sessionId": "ses_test", "update": {
            "sessionUpdate": "session_info_update", "_meta": { "opencode/retry": null }
        } }),
            &tools,
            &streams,
            &events,
        )
        .await;
        assert!(drain_notifications(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn completed_turn_status_is_bounded_and_never_marks_a_new_prompt_idle() {
        let (outbound, mut requests) = mpsc::unbounded_channel();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            inbound: Arc::default(),
            pending: Arc::default(),
            detached: Arc::default(),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: mpsc::unbounded_channel().0,
        };
        let mut ids = Vec::new();
        for _ in 0..=COMPLETED_TURN_LIMIT {
            let turn = client.start_prompt("ses_1", vec![], false).await.unwrap();
            let request = requests.recv().await.unwrap();
            route_message(
                json!({ "id": request["id"], "result": {} }),
                &client.pending,
                &client.detached,
                &client.active_turns,
                &Arc::default(),
                &Arc::default(),
                &Arc::default(),
                &client.events,
                &client.inbound,
                &client.outbound,
            )
            .await;
            ids.push(turn);
        }
        assert_eq!(
            client.active_turns.lock().await.completed.len(),
            COMPLETED_TURN_LIMIT
        );
        assert_eq!(
            client.turn_status("ses_1", Some(&ids[0])).await,
            json!({ "known": false })
        );
        let last = ids.last().unwrap();
        assert_eq!(
            client.turn_status("ses_1", Some(last)).await["running"],
            false
        );
        assert_eq!(
            client.turn_status("other-session", Some(last)).await,
            json!({ "known": false })
        );

        let next = client.start_prompt("ses_1", vec![], false).await.unwrap();
        assert_eq!(
            client.turn_status("ses_1", Some(last)).await,
            json!({ "known": true, "running": true, "turnId": next })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_health_stops_waiting_when_the_server_never_responds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = ProviderAuthServer::new(listener.local_addr().unwrap().port()).unwrap();
        let waiting = tokio::spawn(async move {
            timeout(Duration::from_secs(130), auth.wait_until_ready()).await
        });
        let (first, _) = listener.accept().await.unwrap();
        let silent = tokio::spawn(async move {
            let mut connections = vec![first];
            while let Ok((stream, _)) = listener.accept().await {
                connections.push(stream);
            }
        });

        let result = waiting.await.unwrap();
        silent.abort();
        assert!(
            result.is_ok(),
            "개별 health 요청이 전체 대기 상한보다 먼저 끝나야 한다"
        );
        assert!(
            result.unwrap().is_err(),
            "응답 없는 서버는 준비 완료로 처리하지 않는다"
        );
    }

    #[tokio::test]
    async fn provider_health_retries_after_an_unresponsive_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = ProviderAuthServer::new(listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            let (_silent, _) = listener.accept().await.unwrap();
            let (recovered, _) = listener.accept().await.unwrap();
            let mut recovered = BufReader::new(recovered);
            let mut line = String::new();
            loop {
                line.clear();
                recovered.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            recovered
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });

        let result = timeout(Duration::from_secs(5), auth.wait_until_ready()).await;
        server.abort();
        assert!(
            result.is_ok(),
            "첫 연결이 멈춰도 다음 health 요청으로 복구해야 한다"
        );
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn provider_body_timeout_is_reported_instead_of_becoming_an_empty_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = ProviderAuthServer::new(listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let result = auth
            .send_json(
                auth.client
                    .get(auth.url(&["provider"]).unwrap())
                    .timeout(Duration::from_millis(100)),
            )
            .await;
        server.abort();
        assert!(
            result.is_err(),
            "본문이 오지 않으면 정상 빈 응답으로 숨기지 않는다"
        );
    }

    #[test]
    fn websearch_tool_updates_become_web_search_items() {
        let started = json!({ "toolCallId": "t1", "title": "websearch", "kind": "other", "rawInput": {} });
        assert_eq!(tool_item(&started, false)["type"], "webSearch");
        let completed = json!({
            "toolCallId": "t1",
            "title": "Exa Web Search \"rust async\"",
            "kind": "other",
            "rawInput": { "query": "rust async" },
            "status": "completed"
        });
        let item = tool_item(&completed, true);
        assert_eq!(item["type"], "webSearch");
        assert_eq!(item["query"], "rust async");
        let other = json!({ "toolCallId": "t2", "title": "lookup", "kind": "other", "rawInput": { "query": "x" } });
        assert_eq!(tool_item(&other, false)["type"], "dynamicToolCall");
    }

    /// 백엔드가 살아 있는 채로 응답만 멈추면 요청을 끊고 알린다. 시간을 멈춘
    /// 검사라 실제로 상한만큼 기다리지 않는다.
    #[tokio::test(start_paused = true)]
    async fn a_silent_backend_ends_the_request_instead_of_hanging() {
        let (outbound, _inbox) = mpsc::unbounded_channel();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            inbound: Arc::default(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            detached: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: mpsc::unbounded_channel().0,
        };

        let error = client
            .request("session/new", json!({}))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("응답하지 않아"), "{error}");
        assert!(
            client.pending.lock().await.is_empty(),
            "끊은 요청은 대기 목록에 남기지 않는다"
        );
    }

    #[test]
    fn open_code_skill_catalog_is_normalized_for_composer_completion() {
        let catalog = normalize_skill_catalog(json!([
            {
                "name": "review",
                "description": "Review a change",
                "location": "C:/skills/review/SKILL.md"
            },
            {
                "name": "built-in",
                "description": "Built in skill"
            }
        ]));
        let skills = catalog
            .pointer("/data/0/skills")
            .and_then(Value::as_array)
            .expect("normalized skills");

        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["path"], "C:/skills/review/SKILL.md");
        assert_eq!(skills[0]["scope"], "provider");
        assert_eq!(skills[1]["path"], "opencode://skill/built-in");
        assert_eq!(skills[1]["scope"], "system");
        assert_eq!(skills[1]["enabled"], true);
    }

    #[test]
    fn open_code_2_skills_use_native_ids_instead_of_display_names() {
        let catalog = normalize_skill_catalog(json!({ "data": [
            { "id": "project-review", "name": "Review", "path": "C:/project/SKILL.md" },
            { "id": "user-review", "name": "Review", "path": "C:/user/SKILL.md" }
        ] }));
        let skills = catalog["data"][0]["skills"].as_array().unwrap();
        assert_eq!(skills[0]["name"], "project-review");
        assert_eq!(skills[1]["name"], "user-review");
        let instruction = explicit_skill_instruction(&[json!({
            "type": "skill", "name": skills[1]["name"], "path": skills[1]["path"]
        })]).unwrap();
        assert!(instruction.contains("user-review"));
        assert!(!instruction.contains("project-review"));
        assert!(!instruction.contains("Review"));
    }

    #[test]
    fn open_code_global_compatible_skills_are_user_owned() {
        let home = env::var_os("USERPROFILE")
            .or_else(|| env::var_os("HOME"))
            .map(PathBuf::from)
            .expect("home");
        let skill = home
            .join(".claude")
            .join("skills")
            .join("personal")
            .join("SKILL.md");

        assert_eq!(open_code_skill_scope(skill.to_str()), "user");
    }

    #[test]
    fn open_code_models_are_namespaced() {
        let model = open_code_model("anthropic/claude-sonnet", "Anthropic/Claude Sonnet", &[]);
        assert_eq!(
            model.get("model").and_then(Value::as_str),
            Some("opencode:anthropic/claude-sonnet")
        );
        assert!(is_open_code_model("opencode:anthropic/claude-sonnet"));
        assert!(!is_open_code_model("gpt-5.6-sol"));
    }

    #[test]
    fn open_code_model_names_put_the_model_before_the_provider() {
        let go = open_code_model(
            "opencode-go/deepseek-v4",
            "OpenCode Go/DeepSeek V4 Flash",
            &[],
        );
        let zen = open_code_model("opencode/deepseek-v4", "OpenCode/DeepSeek V4 Flash", &[]);
        let anthropic = open_code_model("anthropic/claude-sonnet", "Anthropic/Claude Sonnet", &[]);

        assert_eq!(
            go.get("displayName").and_then(Value::as_str),
            Some("DeepSeek V4 Flash · OpenCode Go")
        );
        assert_eq!(
            zen.get("displayName").and_then(Value::as_str),
            Some("DeepSeek V4 Flash · OpenCode Zen")
        );
        assert_eq!(
            anthropic.get("displayName").and_then(Value::as_str),
            Some("Claude Sonnet · Anthropic")
        );
    }

    #[test]
    fn model_variants_become_model_specific_effort_choices() {
        let catalog = json!({
            "all": [{
                "id": "opencode-go",
                "models": {
                    "gpt-5.6-luna": {
                        "variants": {
                            "xhigh": { "reasoningEffort": "xhigh" },
                            "none": { "reasoningEffort": "none" },
                            "medium": { "reasoningEffort": "medium" },
                            "low": { "reasoningEffort": "low" },
                            "high": { "reasoningEffort": "high" },
                            "max": { "reasoningEffort": "max" }
                        }
                    },
                    "muse-spark": {
                        "variants": {
                            "minimal": { "reasoningEffort": "minimal" },
                            "low": { "reasoningEffort": "low" },
                            "high": { "reasoningEffort": "high" }
                        }
                    }
                }
            }]
        });
        let luna = model_reasoning_efforts(&catalog, "opencode-go/gpt-5.6-luna");
        let muse = model_reasoning_efforts(&catalog, "opencode-go/muse-spark");

        assert_eq!(luna, ["none", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(muse, ["minimal", "low", "high"]);
        let model = open_code_model(
            "opencode-go/gpt-5.6-luna",
            "OpenCode Go/GPT-5.6 Luna",
            &luna,
        );
        assert_eq!(
            model.get("defaultReasoningEffort").and_then(Value::as_str),
            Some("none")
        );
        assert_eq!(
            model
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(6)
        );
    }

    #[test]
    fn zen_and_go_authentication_unlocks_both_model_families() {
        let zen_only = vec!["opencode".to_owned()];
        assert!(provider_visible(&zen_only, "opencode"));
        assert!(provider_visible(&zen_only, "opencode-go"));
        assert!(!provider_visible(&zen_only, "xai"));

        let go_only = vec!["opencode-go".to_owned()];
        assert!(provider_visible(&go_only, "opencode"));
        assert!(provider_visible(&go_only, "opencode-go"));
        assert!(!provider_visible(&go_only, "anthropic"));

        assert!(!provider_visible(&[], "openai"));
        assert!(provider_visible(&[], "xai"));
    }

    #[test]
    fn message_stream_splits_at_tool_boundary() {
        let mut streams = SessionStreams::default();
        let first = streams.message_chunk("s", None, "진행 문장");
        assert_eq!(first[0].0, "item/started");
        assert_eq!(first[1].0, "item/agentMessage/delta");
        let boundary = streams.close_all("s");
        assert_eq!(boundary[0].0, "item/completed");
        assert_eq!(
            boundary[0].1.pointer("/item/text").and_then(Value::as_str),
            Some("진행 문장")
        );
        let second = streams.message_chunk("s", None, "최종 답변");
        assert_ne!(
            first[1].1.get("itemId").and_then(Value::as_str),
            second[1].1.get("itemId").and_then(Value::as_str)
        );
    }

    #[test]
    fn thought_boundary_closes_open_message_without_showing_thought() {
        let mut streams = SessionStreams::default();
        streams.message_chunk("s", None, "본문");
        let events = streams.thought_boundary("s");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "item/completed");
        assert_eq!(
            events[0].1.pointer("/item/type").and_then(Value::as_str),
            Some("agentMessage")
        );
        // 사고만 이어지는 동안에는 아무것도 내보내지 않는다.
        assert!(streams.thought_boundary("s").is_empty());
    }

    #[test]
    fn changed_message_id_starts_new_item() {
        let mut streams = SessionStreams::default();
        streams.message_chunk("s", Some("m1"), "첫 메시지");
        let events = streams.message_chunk("s", Some("m2"), "둘째 메시지");
        assert_eq!(events[0].0, "item/completed");
        assert_eq!(
            events[0].1.pointer("/item/text").and_then(Value::as_str),
            Some("첫 메시지")
        );
        assert_eq!(events[1].0, "item/started");
        // 같은 메시지 id가 이어지면 항목을 나누지 않는다.
        let same = streams.message_chunk("s", Some("m2"), " 계속");
        assert_eq!(same.len(), 1);
        assert_eq!(same[0].0, "item/agentMessage/delta");
    }

    #[test]
    fn task_tool_calls_become_subagent_rows() {
        let mut streams = SessionStreams::default();
        let update = json!({
            "toolCallId": "call_1",
            "title": "task",
            "kind": "other",
            "rawInput": { "subagent_type": "explore", "description": "코드 조사" }
        });

        let started = streams
            .subagent_started("s", &update)
            .expect("task 도구는 하위 에이전트 행이 된다");
        assert_eq!(started.0, "turn/subagents/updated");
        assert_eq!(
            started.1.pointer("/subagents/0/id").and_then(Value::as_str),
            Some("call_1")
        );
        assert_eq!(
            started
                .1
                .pointer("/subagents/0/name")
                .and_then(Value::as_str),
            Some("explore")
        );
        assert_eq!(
            started
                .1
                .pointer("/subagents/0/description")
                .and_then(Value::as_str),
            Some("코드 조사")
        );

        // 같은 id가 다시 시작돼도 행이 늘지 않는다.
        let repeated = streams
            .subagent_started("s", &update)
            .expect("갱신도 행 하나로 남는다");
        assert_eq!(
            repeated
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        let finished = streams
            .subagent_finished("s", "call_1")
            .expect("완료는 행을 지운다");
        assert_eq!(
            finished
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        // 이미 지운 id는 알림을 다시 만들지 않는다.
        assert!(streams.subagent_finished("s", "call_1").is_none());
    }

    #[test]
    fn ordinary_tools_never_become_subagent_rows() {
        assert!(
            subagent_entry(&json!({
                "toolCallId": "call_2",
                "title": "grep",
                "rawInput": { "pattern": "fn main" }
            }))
            .is_none()
        );
        // 제목이 task이면 subagent_type이 없어도 하위 에이전트로 본다.
        assert!(
            subagent_entry(&json!({
                "toolCallId": "call_3",
                "title": "Task",
                "rawInput": {}
            }))
            .is_some()
        );
        // id가 없으면 행을 만들 수 없다.
        assert!(
            subagent_entry(&json!({
                "title": "task",
                "rawInput": { "subagent_type": "explore" }
            }))
            .is_none()
        );
    }

    #[test]
    fn turn_end_clears_remaining_subagent_rows() {
        let mut streams = SessionStreams::default();
        streams.subagent_started(
            "s",
            &json!({
                "toolCallId": "call_1",
                "title": "task",
                "rawInput": { "subagent_type": "developer", "description": "구현" }
            }),
        );

        let cleared = streams
            .subagents_cleared("s")
            .expect("턴이 끝나면 남은 행을 거둔다");
        assert_eq!(
            cleared
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        // 비어 있으면 알림을 반복하지 않는다.
        assert!(streams.subagents_cleared("s").is_none());
    }

    #[test]
    fn background_subagent_waits_for_child_session_status() {
        let mut streams = SessionStreams::default();
        streams.subagent_started(
            "s",
            &json!({
                "toolCallId": "call_1",
                "title": "task",
                "rawInput": { "subagent_type": "developer", "description": "구현" }
            }),
        );

        let kept = streams
            .subagent_tool_completed(
                "s",
                "call_1",
                &json!({
                    "rawOutput": {
                        "metadata": {
                            "background": true,
                            "sessionId": "child_1",
                            "jobId": "job_1"
                        }
                    }
                }),
            )
            .expect("바깥 도구가 끝나도 background 행은 유지한다");
        assert_eq!(
            kept.1
                .pointer("/subagents/0/taskId")
                .and_then(Value::as_str),
            Some("child_1")
        );
        assert!(streams.subagents_cleared("s").is_none());

        let first_missing = streams
            .reconcile_background_subagents("s", &json!({}))
            .expect("관측 전 첫 누락은 시작 경합으로 보고 행을 유지한다");
        assert_eq!(
            first_missing
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        let quick_finish = streams
            .reconcile_background_subagents("s", &json!({}))
            .expect("관측 전에도 연속 누락이면 빠른 종료로 확정한다");
        assert_eq!(
            quick_finish
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );

        streams.subagent_started(
            "s",
            &json!({ "toolCallId": "call_2", "title": "task", "rawInput": {} }),
        );
        streams.subagent_tool_completed(
            "s",
            "call_2",
            &json!({
                "rawOutput": { "metadata": { "background": true, "sessionId": "child_2" } }
            }),
        );
        let active = streams
            .reconcile_background_subagents("s", &json!({ "child_2": { "type": "busy" } }))
            .expect("실행 중 상태는 heartbeat를 보낸다");
        assert_eq!(
            active
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        let observed_finish = streams
            .reconcile_background_subagents("s", &json!({}))
            .expect("실행이 확인된 자식은 첫 누락에 바로 제거한다");
        assert_eq!(
            observed_finish
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );

        streams.subagent_started(
            "s",
            &json!({ "toolCallId": "call_3", "title": "task", "rawInput": {} }),
        );
        streams.subagent_tool_completed(
            "s",
            "call_3",
            &json!({
                "rawOutput": { "metadata": { "background": true, "sessionId": "child_3" } }
            }),
        );
        let explicit_idle = streams
            .reconcile_background_subagents("s", &json!({ "child_3": { "type": "idle" } }))
            .expect("명시적 idle은 첫 상태 확인에서 제거한다");
        assert!(
            explicit_idle
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        );

        streams.subagent_started(
            "s",
            &json!({ "toolCallId": "call_4", "title": "task", "rawInput": {} }),
        );
        let cleared = streams
            .all_subagents_cleared("s")
            .expect("전송 연결 종료는 모든 행을 제거한다");
        assert!(
            cleared
                .1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        );
    }

    #[test]
    fn replayed_history_becomes_turns_without_injected_rules() {
        let mut chunks = Vec::new();
        record_history_chunk(
            &mut chunks,
            &json!({
                "sessionUpdate": "user_message_chunk",
                "content": {
                    "type": "text",
                    "text": "<devez-vibe-rules>\n규칙\n</devez-vibe-rules>"
                }
            }),
        );
        record_history_chunk(
            &mut chunks,
            &json!({
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "질문" }
            }),
        );
        record_history_chunk(
            &mut chunks,
            &json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "답" }
            }),
        );
        record_history_chunk(
            &mut chunks,
            &json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "변" }
            }),
        );
        record_history_chunk(
            &mut chunks,
            &json!({
                "sessionUpdate": "tool_call",
                "content": { "type": "text", "text": "무시" }
            }),
        );

        let turns = history_turns("ses_1", &chunks, None);
        assert_eq!(turns.len(), 1);
        let items = turns[0]
            .get("items")
            .and_then(Value::as_array)
            .expect("턴에는 항목이 있다");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].get("type").and_then(Value::as_str),
            Some("userMessage")
        );
        assert_eq!(
            items[0].pointer("/content/0/text").and_then(Value::as_str),
            Some("질문")
        );
        assert_eq!(
            items[1].get("type").and_then(Value::as_str),
            Some("agentMessage")
        );
        assert_eq!(items[1].get("text").and_then(Value::as_str), Some("답변"));
    }

    #[tokio::test]
    async fn steer_joins_the_running_turn_and_completes_once() {
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound_tx))),
            inbound: Arc::default(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            detached: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: event_tx.clone(),
        };
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));

        let first = client
            .start_prompt(
                "ses_1",
                vec![json!({ "type": "text", "text": "질문" })],
                false,
            )
            .await
            .expect("첫 prompt");
        assert_eq!(
            client.turn_status("absent", None).await,
            json!({ "known": false })
        );
        assert_eq!(
            client.turn_status("ses_1", Some(&first)).await,
            json!({ "known": true, "running": true, "turnId": first })
        );
        let steered = client
            .start_prompt(
                "ses_1",
                vec![json!({ "type": "text", "text": "추가 지시" })],
                true,
            )
            .await
            .expect("추가 지시 prompt");
        // 추가 지시는 새 턴을 만들지 않고 같은 턴에 합류한다.
        assert_eq!(first, steered);

        let first_request = outbound_rx.recv().await.expect("첫 요청");
        let second_request = outbound_rx.recv().await.expect("둘째 요청");
        let first_id = first_request.get("id").and_then(Value::as_u64).expect("id");
        let second_id = second_request
            .get("id")
            .and_then(Value::as_u64)
            .expect("id");

        // turn/started는 첫 prompt에서 한 번만 나온다.
        let started = event_rx.recv().await.expect("turn/started");
        assert!(matches!(
            &started,
            ServerEvent::Notification { method, .. } if method == "turn/started"
        ));
        assert!(event_rx.try_recv().is_err());

        // 첫 prompt가 끝나도 추가 지시가 남아 있으면 턴은 끝나지 않는다.
        route_message(
            json!({ "jsonrpc": "2.0", "id": first_id, "result": {} }),
            &client.pending,
            &client.detached,
            &client.active_turns,
            &loading,
            &tools,
            &streams,
            &event_tx,
            &client.inbound,
            &client.outbound,
        )
        .await;
        assert!(event_rx.try_recv().is_err());

        assert_eq!(
            client.turn_status("ses_1", Some(&first)).await["running"],
            true
        );

        // 마지막 응답에서만 turn/completed가 한 번 나온다.
        route_message(
            json!({ "jsonrpc": "2.0", "id": second_id, "result": {} }),
            &client.pending,
            &client.detached,
            &client.active_turns,
            &loading,
            &tools,
            &streams,
            &event_tx,
            &client.inbound,
            &client.outbound,
        )
        .await;
        let completed = event_rx.recv().await.expect("turn/completed");
        assert_eq!(
            client.turn_status("ses_1", Some(&first)).await,
            json!({ "known": true, "running": false, "turnId": first })
        );
        assert_eq!(
            client.turn_status("ses_1", Some("unknown-turn")).await,
            json!({ "known": false })
        );
        assert_eq!(
            client.turn_status("ses_1", Some("")).await,
            json!({ "known": false })
        );
        assert_eq!(
            client.turn_status("ses_1", None).await,
            json!({ "known": false })
        );
        match completed {
            ServerEvent::Notification { method, params } => {
                assert_eq!(method, "turn/completed");
                assert_eq!(
                    params.pointer("/turn/id").and_then(Value::as_str),
                    Some(first.as_str())
                );
            }
            _ => panic!("turn/completed 알림이어야 한다"),
        }
        assert!(event_rx.try_recv().is_err());
    }

    fn drain_notifications(rx: &mut mpsc::UnboundedReceiver<ServerEvent>) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ServerEvent::Notification { method, params } = event {
                out.push((method, params));
            }
        }
        out
    }

    #[tokio::test]
    async fn subagent_rows_follow_tool_completion_and_turn_end() {
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound_tx))),
            inbound: Arc::default(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            detached: Arc::new(Mutex::new(HashMap::new())),
            active_turns: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: event_tx.clone(),
        };
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        let route = |message: Value| {
            route_message(
                message,
                &client.pending,
                &client.detached,
                &client.active_turns,
                &loading,
                &tools,
                &streams,
                &event_tx,
                &client.inbound,
                &client.outbound,
            )
        };

        let turn = client
            .start_prompt(
                "ses_1",
                vec![json!({ "type": "text", "text": "작업" })],
                false,
            )
            .await
            .expect("prompt");
        let request_id = outbound_rx
            .recv()
            .await
            .and_then(|request| request.get("id").and_then(Value::as_u64))
            .expect("prompt 요청 id");
        drain_notifications(&mut event_rx);

        // 첫 task 도구: 시작하면 행이 생기고, 완료 신호가 오면 행이 사라진다.
        route(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "task_1",
                    "title": "task",
                    "kind": "other",
                    "status": "pending",
                    "rawInput": { "subagent_type": "developer", "description": "구현" }
                }
            }
        }))
        .await;
        let started = drain_notifications(&mut event_rx);
        let rows = started
            .iter()
            .find(|(method, _)| method == "turn/subagents/updated")
            .expect("시작 시 하위 에이전트 행 알림");
        assert_eq!(
            rows.1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        route(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "task_1",
                    "status": "completed",
                    "rawOutput": { "output": "끝" }
                }
            }
        }))
        .await;
        let finished = drain_notifications(&mut event_rx);
        let rows = finished
            .iter()
            .find(|(method, _)| method == "turn/subagents/updated")
            .expect("완료 시 행 제거 알림");
        assert_eq!(
            rows.1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );

        // 둘째 task는 완료 신호를 놓친다. 턴이 끝나면 행을 거둔 뒤에
        // turn/completed가 나와야 화면에 도는 행이 남지 않는다.
        route(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "task_2",
                    "title": "task",
                    "kind": "other",
                    "status": "pending",
                    "rawInput": { "subagent_type": "qa-tester", "description": "검증" }
                }
            }
        }))
        .await;
        drain_notifications(&mut event_rx);
        route(json!({ "jsonrpc": "2.0", "id": request_id, "result": {} })).await;
        let ended = drain_notifications(&mut event_rx);
        let cleared_index = ended
            .iter()
            .position(|(method, params)| {
                method == "turn/subagents/updated"
                    && params
                        .pointer("/subagents")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
            })
            .expect("턴 종료가 남은 행을 거둔다");
        let completed_index = ended
            .iter()
            .position(|(method, params)| {
                method == "turn/completed"
                    && params.pointer("/turn/id").and_then(Value::as_str) == Some(turn.as_str())
            })
            .expect("턴 종료 알림");
        assert!(cleared_index < completed_index);
    }

    #[tokio::test]
    async fn a_failed_task_tool_also_removes_its_subagent_row() {
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();

        route_session_update(
            json!({
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "task_1",
                    "title": "task",
                    "status": "pending",
                    "rawInput": { "subagent_type": "developer", "description": "구현" }
                }
            }),
            &tools,
            &streams,
            &event_tx,
        )
        .await;
        drain_notifications(&mut event_rx);

        route_session_update(
            json!({
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "task_1",
                    "status": "failed",
                    "rawOutput": { "error": "실패" }
                }
            }),
            &tools,
            &streams,
            &event_tx,
        )
        .await;
        let events = drain_notifications(&mut event_rx);
        let rows = events
            .iter()
            .find(|(method, _)| method == "turn/subagents/updated")
            .expect("실패도 행을 제거한다");
        assert_eq!(
            rows.1
                .pointer("/subagents")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn completed_background_task_keeps_its_subagent_row() {
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();

        route_session_update(
            json!({
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "task_1",
                    "title": "task",
                    "status": "pending",
                    "rawInput": { "subagent_type": "developer", "description": "구현" }
                }
            }),
            &tools,
            &streams,
            &event_tx,
        )
        .await;
        drain_notifications(&mut event_rx);

        route_session_update(
            json!({
                "sessionId": "ses_1",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "task_1",
                    "status": "completed",
                    "rawOutput": {
                        "metadata": {
                            "background": true,
                            "sessionId": "child_1",
                            "jobId": "child_1"
                        }
                    }
                }
            }),
            &tools,
            &streams,
            &event_tx,
        )
        .await;
        let events = drain_notifications(&mut event_rx);
        let rows = events
            .iter()
            .find(|(method, _)| method == "turn/subagents/updated")
            .expect("background 완료는 행 유지 알림을 보낸다");
        assert_eq!(
            rows.1
                .pointer("/subagents/0/taskId")
                .and_then(Value::as_str),
            Some("child_1")
        );
        assert_eq!(
            rows.1
                .pointer("/subagents/0/background")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn open_code_2_subagent_rows_follow_child_session_updates() {
        let route_inbound: InboundMap = Arc::default();
        let route_outbound: Outbound = Arc::default();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let detached: DetachedMap = Arc::new(Mutex::new(HashMap::new()));
        let active: ActiveMap = Arc::default();
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let route = |message: Value| {
            route_message(
                message, &pending, &detached, &active, &loading, &tools, &streams, &event_tx,
                &route_inbound,
                &route_outbound,
            )
        };
        let update = |update: Value| {
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": "ses_1", "update": update }
            })
        };
        let rows = |events: &[(String, Value)]| {
            events
                .iter()
                .rev()
                .find(|(method, _)| method == "turn/subagents/updated")
                .map(|(_, params)| params["subagents"].clone())
        };

        // 2.x는 빈 입력으로 시작하고 다음 갱신에서 에이전트를 알린다.
        route(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "name": "subagent",
            "title": "subagent",
            "kind": "think",
            "status": "pending",
            "rawInput": {}
        })))
        .await;
        assert_eq!(
            rows(&drain_notifications(&mut event_rx)).unwrap()[0]["name"],
            "agent"
        );
        route(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "in_progress",
            "rawInput": { "agent": "explore", "description": "코드 조사", "background": true }
        })))
        .await;
        let described = rows(&drain_notifications(&mut event_rx)).expect("에이전트 이름 갱신");
        assert_eq!(described[0]["name"], "explore");
        assert_eq!(described[0]["description"], "코드 조사");

        route(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "rawOutput": { "metadata": { "sessionID": "ses_child", "status": "running" } }
        })))
        .await;
        let kept = rows(&drain_notifications(&mut event_rx)).expect("백그라운드 행 유지");
        assert_eq!(kept[0]["taskId"], "ses_child");

        // 자식의 본문은 부모 답변 대신 부모 도구 행의 기록으로만 보낸다.
        let child = |event: Value| {
            let mut params = json!({
                "rootSessionId": "ses_1",
                "childSessionId": "ses_child",
                "parentSessionId": "ses_1",
                "depth": 1
            });
            params
                .as_object_mut()
                .unwrap()
                .extend(event.as_object().unwrap().clone());
            json!({ "jsonrpc": "2.0", "method": "opencode/session/child_update", "params": params })
        };
        route(child(json!({
            "type": "update",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_child",
                "content": { "type": "text", "text": "자식 답변" }
            }
        })))
        .await;
        let history = drain_notifications(&mut event_rx);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].0, "turn/subagent/history");
        assert_eq!(history[0].1["threadId"], "ses_1");
        assert_eq!(history[0].1["id"], "call_1");
        assert_eq!(history[0].1["turns"][0]["items"][0]["text"], "자식 답변");

        route(child(json!({
            "type": "update",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "ses_child:tool_1",
                "kind": "execute",
                "title": "shell",
                "rawInput": { "command": "cargo check" }
            }
        })))
        .await;
        drain_notifications(&mut event_rx);
        route(child(json!({
            "type": "update",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "ses_child:tool_1",
                "status": "completed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "검사 통과" } }]
            }
        }))).await;
        let history = drain_notifications(&mut event_rx);
        let items = history[0].1["turns"][0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["text"], "자식 답변");
        assert_eq!(items[1]["command"], "cargo check");
        assert_eq!(items[1]["aggregatedOutput"], "검사 통과");
        assert_eq!(items[1]["status"], "completed");
        route(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "msg_child_next",
            "_meta": { "opencode/child-session": { "id": "ses_child", "parentID": "ses_1", "depth": 1 } },
            "content": { "type": "text", "text": "메타 경로 답변" }
        }))).await;
        let history = drain_notifications(&mut event_rx);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].0, "turn/subagent/history");
        assert_eq!(
            history[0].1["turns"][0]["items"][2]["text"],
            "메타 경로 답변"
        );
        route(child(json!({
            "type": "update",
            "update": { "sessionUpdate": "tool_call", "toolCallId": "ses_child:pending", "kind": "execute", "title": "shell" }
        }))).await;
        drain_notifications(&mut event_rx);
        route(child(json!({ "type": "status", "status": "completed" }))).await;
        let ended = rows(&drain_notifications(&mut event_rx)).expect("자식 종료 시 행 제거");
        assert_eq!(ended.as_array().map(Vec::len), Some(0));
        let streams = streams.lock().await;
        assert!(!streams.contains_key("ses_child"));
        assert!(streams["ses_1"].child_tool_ids.is_empty());
        assert!(!tools.lock().await.contains_key("ses_child:pending"));
    }

    #[tokio::test]
    async fn child_history_waits_for_a_confirmed_parent_tool_session_id() {
        let tools: ToolMap = Arc::default();
        let streams: StreamMap = Arc::default();
        let (events, mut receiver) = mpsc::unbounded_channel();
        route_child_update(
            "root",
            "child",
            Some(&json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "연결 전 기록" }
            })),
            true,
            &tools,
            &streams,
            &events,
        )
        .await;
        assert!(drain_notifications(&mut receiver).is_empty());
        link_child_session("root", &json!({
            "name": "task", "toolCallId": "v1", "rawOutput": { "metadata": { "sessionId": "child" } }
        }), &streams, &events).await;
        assert!(drain_notifications(&mut receiver).is_empty());
        link_child_session("root", &json!({
            "name": "subagent", "toolCallId": "foreground", "rawOutput": { "metadata": { "sessionID": "child" } }
        }), &streams, &events).await;
        let history = drain_notifications(&mut receiver);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1["id"], "foreground");
        assert_eq!(history[0].1["turns"][0]["items"][0]["text"], "연결 전 기록");
        let streams = streams.lock().await;
        assert!(!streams.contains_key("child"));
        assert!(streams["root"].child_tool_ids.is_empty());
    }

    #[tokio::test]
    async fn completed_child_history_has_count_and_size_limits_before_parent_link() {
        let tools: ToolMap = Arc::default();
        let streams: StreamMap = Arc::default();
        let (events, _receiver) = mpsc::unbounded_channel();
        for index in 0..COMPLETED_CHILD_LIMIT + 2 {
            route_child_update(
                "root",
                &format!("child_{index}"),
                Some(&json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "연결 대기" }
                })),
                true,
                &tools,
                &streams,
                &events,
            )
            .await;
        }
        {
            let streams = streams.lock().await;
            assert_eq!(streams.len(), COMPLETED_CHILD_LIMIT);
            assert!(!streams.contains_key("child_0"));
            assert!(streams.contains_key(&format!("child_{}", COMPLETED_CHILD_LIMIT + 1)));
        }
        route_child_update(
            "root",
            "oversized",
            Some(&json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "x".repeat(COMPLETED_CHILD_BYTES + 1) }
            })),
            true,
            &tools,
            &streams,
            &events,
        )
        .await;
        assert!(streams.lock().await.is_empty());
    }

    #[tokio::test]
    async fn completed_parent_metadata_releases_child_without_a_terminal_notification() {
        let tools: ToolMap = Arc::default();
        let streams: StreamMap = Arc::default();
        let (events, mut receiver) = mpsc::unbounded_channel();
        route_child_update("root", "child", Some(&json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "종료 알림 없는 기록" }
        })), false, &tools, &streams, &events).await;
        assert!(
            link_child_session(
                "root",
                &json!({
                    "name": "subagent", "toolCallId": "parent",
                    "rawOutput": { "metadata": { "sessionID": "child", "status": "completed" } }
                }),
                &streams,
                &events
            )
            .await
        );
        assert_eq!(
            drain_notifications(&mut receiver)[0].1["turns"][0]["items"][0]["text"],
            "종료 알림 없는 기록"
        );
        let streams = streams.lock().await;
        assert!(!streams.contains_key("child"));
        assert!(streams["root"].child_tool_ids.is_empty());
    }

    #[tokio::test]
    async fn reused_child_history_keeps_parent_and_current_tool_separate() {
        let tools: ToolMap = Arc::default();
        let streams: StreamMap = Arc::default();
        let (events, mut receiver) = mpsc::unbounded_channel();
        let link = |child: &str, tool: &str| {
            json!({
                "name": "subagent", "toolCallId": tool, "rawInput": { "sessionID": child }
            })
        };
        link_child_session("root_a", &link("child_a", "old"), &streams, &events).await;
        route_child_update("root_a", "child_a", Some(&json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "지난 실행" }
        })), true, &tools, &streams, &events).await;
        let history = drain_notifications(&mut receiver);
        assert_eq!(history[0].1["id"], "old");
        assert!(!streams.lock().await.contains_key("child_a"));
        link_child_session("root_a", &link("child_a", "new"), &streams, &events).await;
        route_child_update("root_a", "child_a", Some(&json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "새 실행" }
        })), false, &tools, &streams, &events).await;
        let history = drain_notifications(&mut receiver);
        assert_eq!(history[0].1["id"], "new");
        assert_eq!(
            history[0].1["turns"][0]["items"].as_array().unwrap().len(),
            1
        );
        assert_eq!(history[0].1["turns"][0]["items"][0]["text"], "새 실행");

        route_child_update("root_b", "child_b", Some(&json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "다른 부모" }
        })), true, &tools, &streams, &events).await;
        assert!(!link_child_session("root_a", &json!({
            "name": "subagent", "toolCallId": "wrong", "rawOutput": { "metadata": { "sessionID": "child_b" } }
        }), &streams, &events).await);
        assert!(drain_notifications(&mut receiver).is_empty());
        assert!(link_child_session("root_b", &json!({
            "name": "subagent", "toolCallId": "b", "rawOutput": { "metadata": { "sessionID": "child_b" } }
        }), &streams, &events).await);
        let history = drain_notifications(&mut receiver);
        assert_eq!(history[0].1["threadId"], "root_b");
        assert_eq!(history[0].1["id"], "b");
        let streams = streams.lock().await;
        assert!(!streams.contains_key("child_b"));
        assert_eq!(
            streams["root_a"]
                .child_tool_ids
                .get("child_a")
                .map(String::as_str),
            Some("new")
        );
        assert!(streams["root_b"].child_tool_ids.is_empty());
    }

    #[tokio::test]
    async fn child_ending_before_parent_result_does_not_leave_a_background_row() {
        for resumed in [false, true] {
            let tools: ToolMap = Arc::default();
            let streams: StreamMap = Arc::default();
            let (events, mut receiver) = mpsc::unbounded_channel();
            route_session_update(json!({
            "sessionId": "root", "update": {
                "sessionUpdate": "tool_call", "toolCallId": "parent", "name": "subagent", "title": "subagent",
                "rawInput": if resumed { json!({ "sessionID": "child" }) } else { json!({}) }
            }
        }), &tools, &streams, &events).await;
            drain_notifications(&mut receiver);
            route_child_update("root", "child", Some(&json!({
            "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "먼저 종료" }
        })), true, &tools, &streams, &events).await;
            route_session_update(json!({
            "sessionId": "root", "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "parent", "status": "completed",
                "rawOutput": { "metadata": { "sessionID": "child", "status": "running" } }
            }
        }), &tools, &streams, &events).await;
            let notifications = drain_notifications(&mut receiver);
            assert!(
                notifications
                    .iter()
                    .any(|(method, params)| method == "turn/subagent/history"
                        && params["id"] == "parent")
            );
            let streams = streams.lock().await;
            assert!(!streams.contains_key("child"));
            assert!(streams["root"].subagents.is_empty());
            assert!(streams["root"].child_tool_ids.is_empty());
            assert!(tools.lock().await.is_empty());
        }
    }

    #[test]
    fn open_code_2_auth_methods_take_the_picker_shape() {
        let methods = [
            json!({ "type": "env", "names": ["AZURE_API_KEY"] }),
            json!({ "type": "key", "form": [{
                "key": "resourceName",
                "title": "Enter Azure Resource Name",
                "type": "string",
                "placeholder": "e.g. my-models"
            }] }),
            json!({ "id": "device", "type": "oauth", "label": "Login", "form": [
                { "key": "deploymentType", "type": "string", "options": [
                    { "value": "github.com", "label": "GitHub.com" },
                    { "value": "enterprise", "label": "Enterprise", "description": "Self-hosted" }
                ] },
                { "key": "enterpriseUrl", "title": "URL", "type": "string",
                  "when": [{ "key": "deploymentType", "op": "eq", "value": "enterprise" }] },
                { "key": "token", "type": "string", "hidden": true }
            ] }),
        ];
        let translated = methods.iter().map(v2_auth_method).collect::<Vec<_>>();

        // 피커가 건너뛰는 유형도 남겨 두어야 방식 위치가 2.x 목록과 같다.
        assert_eq!(translated[0]["type"], "env");
        assert_eq!(
            translated[1],
            json!({ "type": "api", "label": "API key", "prompts": [{
                "type": "text",
                "key": "resourceName",
                "message": "Enter Azure Resource Name",
                "placeholder": "e.g. my-models",
                "when": null
            }] })
        );
        let prompts = translated[2]["prompts"].as_array().expect("OAuth 입력");
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0]["type"], "select");
        assert_eq!(prompts[0]["options"][1]["hint"], "Self-hosted");
        assert_eq!(
            prompts[1]["when"],
            json!({ "key": "deploymentType", "op": "eq", "value": "enterprise" })
        );
    }

    #[tokio::test]
    async fn session_updates_during_load_fill_the_history_buffer() {
        let route_inbound: InboundMap = Arc::default();
        let route_outbound: Outbound = Arc::default();
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let detached: DetachedMap = Arc::new(Mutex::new(HashMap::new()));
        let active: ActiveMap = Arc::default();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        loading
            .lock()
            .await
            .insert("ses_1".to_owned(), LoadedHistory::default());

        route_message(
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "ses_1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "복원된 답변" }
                    }
                }
            }),
            &pending,
            &detached,
            &active,
            &loading,
            &tools,
            &streams,
            &event_tx,
            &route_inbound,
            &route_outbound,
        )
        .await;

        // 재생 조각은 화면 알림 대신 복원 버퍼로 간다.
        assert!(event_rx.try_recv().is_err());
        let buffered = loading.lock().await.remove("ses_1").expect("버퍼");
        assert_eq!(buffered.chunks.len(), 1);
        assert!(!buffered.chunks[0].user);
        assert_eq!(buffered.chunks[0].text, "복원된 답변");
    }

    #[tokio::test]
    async fn usage_updates_during_load_restore_context_immediately() {
        let route_inbound: InboundMap = Arc::default();
        let route_outbound: Outbound = Arc::default();
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let detached: DetachedMap = Arc::new(Mutex::new(HashMap::new()));
        let active: ActiveMap = Arc::default();
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        loading
            .lock()
            .await
            .insert("ses_1".to_owned(), LoadedHistory::default());

        route_message(
            json!({
                "method": "session/update",
                "params": {
                    "sessionId": "ses_1",
                    "update": { "sessionUpdate": "usage_update", "used": 42_000, "size": 200_000 }
                }
            }),
            &pending,
            &detached,
            &active,
            &loading,
            &tools,
            &streams,
            &event_tx,
            &route_inbound,
            &route_outbound,
        )
        .await;

        let loaded = loading.lock().await.remove("ses_1").expect("버퍼");
        let usage = loaded.token_usage.expect("사용량");
        assert_eq!(usage["last"]["totalTokens"], 42_000);
        assert_eq!(usage["modelContextWindow"], 200_000);
    }

    #[test]
    fn native_plan_and_todo_updates_keep_times_in_live_and_restored_state() {
        for update in [
            json!({ "sessionUpdate": "plan", "entries": [
                { "id": "1", "content": "조사", "status": "in_progress" },
                { "id": "2", "text": "검증", "status": "completed", "elapsedMs": 6000 },
                { "content": 42, "status": "completed" }
            ] }),
            json!({ "sessionUpdate": "tool_call", "title": "todowrite", "rawInput": { "todos": [
                { "id": "1", "content": "조사", "status": "active" },
                { "id": "2", "content": "검증", "status": "done", "elapsedMs": 6000 },
                { "content": "", "status": "completed" }
            ] } }),
        ] {
            let plan = plan_update(&update).unwrap();
            assert_eq!(plan.len(), 2);
            let mut state = crate::state::AppState::new(
                "ses_1".into(),
                "cwd".into(),
                "시험".into(),
                Vec::new(),
                "opencode:test/test",
                None,
            );
            state.handle_notification("turn/plan/updated", &json!({ "plan": plan }));
            assert_eq!(
                state.view().plan_summary.unwrap().steps[1].elapsed,
                Some(Duration::from_secs(6))
            );
            let turns = history_turns("ses_1", &[], Some(&plan));
            state.prepare_resume();
            state.load_history(&json!({ "turns": turns }), None);
            assert_eq!(
                state.view().plan_summary.unwrap().steps[1].elapsed,
                Some(Duration::from_secs(6))
            );
        }
    }

    #[tokio::test]
    async fn plan_updates_during_load_are_buffered_instead_of_discarded() {
        let route_inbound: InboundMap = Arc::default();
        let route_outbound: Outbound = Arc::default();
        let loading: LoadingMap = Arc::new(Mutex::new(HashMap::new()));
        loading
            .lock()
            .await
            .insert("ses_1".to_owned(), LoadedHistory::default());
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        route_message(
            json!({ "method": "session/update", "params": {
            "sessionId": "ses_1", "update": { "sessionUpdate": "plan", "entries": [
                { "content": "검증", "status": "completed", "elapsedMs": 6000 }
            ] }
        } }),
            &Arc::default(),
            &Arc::default(),
            &Arc::default(),
            &loading,
            &Arc::default(),
            &Arc::default(),
            &event_tx,
            &route_inbound,
            &route_outbound,
        )
        .await;
        assert!(event_rx.try_recv().is_err());
        let loaded = loading.lock().await.remove("ses_1").unwrap();
        assert_eq!(loaded.plan.unwrap()[0]["elapsedMs"], 6000);
    }

    #[test]
    fn todo_tool_becomes_plan_statuses() {
        assert_eq!(plan_status(Some("in_progress")), "inProgress");
        assert_eq!(plan_status(Some("completed")), "completed");
        assert_eq!(plan_status(Some("pending")), "pending");
    }

    #[test]
    fn image_extensions_map_to_acp_mime_types() {
        assert_eq!(image_mime(Path::new("screen.webp")), "image/webp");
        assert_eq!(image_mime(Path::new("screen.png")), "image/png");
    }

    #[test]
    fn explicit_skill_items_request_the_native_open_code_skill_tool() {
        let instruction = explicit_skill_instruction(&[
            json!({ "type": "text", "text": "$luna-loop 검증" }),
            json!({ "type": "skill", "name": "luna-loop", "path": "SKILL.md" }),
        ])
        .expect("명시적 Skill 지침");

        assert!(instruction.contains("native skill tool"));
        assert!(instruction.contains("luna-loop"));
    }

    #[test]
    fn permission_decisions_map_to_acp_options() {
        assert_eq!(
            permission_result(&json!({ "decision": "acceptForSession" }))
                .pointer("/outcome/optionId")
                .and_then(Value::as_str),
            Some("always")
        );
        assert_eq!(
            permission_result(&json!({ "permissions": { "opencode": {} }, "scope": "turn" }))
                .pointer("/outcome/optionId")
                .and_then(Value::as_str),
            Some("once")
        );
    }
}
