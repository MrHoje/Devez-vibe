use std::{
    collections::{HashMap, VecDeque},
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

use crate::app_server::ServerEvent;

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;

/// 브리지가 죽지 않고 응답만 멈추면 요청이 영원히 매달려 스피너만 남는다.
/// 세션 시작이나 큰 전사 복원도 이보다 오래 걸리지는 않으므로, 넘기면
/// 멈춘 것으로 보고 사용자에게 알린다. 한도 대기는 턴 알림으로 오가므로
/// 이 대기에 걸리지 않는다.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct ClaudeClient {
    outbound: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    process: Arc<Mutex<Option<ClaudeProcess>>>,
    start_lock: Arc<Mutex<()>>,
    events: mpsc::UnboundedSender<ServerEvent>,
    node_path: PathBuf,
    claude_path: PathBuf,
    bridge_path: PathBuf,
    cwd: PathBuf,
}

struct ClaudeProcess {
    child: Child,
    /// 브리지가 띄운 Claude CLI와 MCP 서버까지 함께 끝낸다.
    _job: Option<crate::child_process::BackendJob>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl ClaudeProcess {
    /// 이미 끊긴 브리지가 남긴 자원을 거둔다. `_job`이 떨어지며 브리지가 띄운
    /// Claude CLI와 MCP 서버까지 함께 끝나므로 고아 프로세스는 남지 않는다.
    async fn discard(mut self) {
        self.writer_task.abort();
        // 읽기 작업은 끝나면서 매달린 요청을 깨우고 종료를 알린다. 곧바로
        // 끊으면 그 정리가 사라져 요청이 상한까지 매달리므로 잠깐 기다린다.
        if timeout(Duration::from_secs(1), &mut self.reader_task)
            .await
            .is_err()
        {
            self.reader_task.abort();
        }
        self.stderr_task.abort();
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(3), self.child.wait()).await;
    }
}

impl ClaudeClient {
    pub async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        if let Some(object) = params.as_object_mut() {
            object.insert(
                "claudePath".to_owned(),
                json!(self.claude_path.to_string_lossy()),
            );
        }
        self.ensure_started().await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);
        if let Err(error) = self.send(json!({ "id": id, "method": method, "params": params })) {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match timeout(REQUEST_TIMEOUT, response_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("{method}: {error}"),
            Ok(Err(_)) => bail!("{method}: Claude SDK 응답 채널이 종료되었습니다."),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!(
                    "{method}: Claude SDK 브리지가 {}분 동안 응답하지 않아 요청을 중단했습니다.",
                    REQUEST_TIMEOUT.as_secs() / 60
                )
            }
        }
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        let id = claude_request_id(&id)?;
        self.send(json!({ "id": id, "result": result }))
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        let id = claude_request_id(&id)?;
        self.send(json!({
            "id": id,
            "error": { "code": code, "message": message }
        }))
    }

    async fn ensure_started(&self) -> Result<()> {
        if self
            .outbound
            .lock()
            .expect("Claude outbound mutex")
            .is_some()
        {
            return Ok(());
        }
        let _guard = self.start_lock.lock().await;
        if self
            .outbound
            .lock()
            .expect("Claude outbound mutex")
            .is_some()
        {
            return Ok(());
        }
        // 브리지가 끊기면 outbound만 비고 프로세스 자리는 남는다. 남은 자리를
        // 치우고 다시 띄워야 앱을 재시작하지 않고도 Claude 요청을 이어간다.
        if let Some(dead) = self.process.lock().await.take() {
            dead.discard().await;
        }

        let mut command = Command::new(&self.node_path);
        command
            .arg(&self.bridge_path)
            .current_dir(&self.cwd)
            .env("DEVEZ_VIBE_VERSION", env!("CARGO_PKG_VERSION"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::child_process::isolate_backend(&mut command);
        let mut child = command.spawn().with_context(|| {
            format!(
                "Claude Agent SDK 브리지를 시작하지 못했습니다: {}",
                self.bridge_path.display()
            )
        })?;
        let job = crate::child_process::adopt_backend(&child);
        let stdin = child.stdin.take().context("Claude SDK stdin 연결 실패")?;
        let stdout = child.stdout.take().context("Claude SDK stdout 연결 실패")?;
        let stderr = child.stderr.take().context("Claude SDK stderr 연결 실패")?;
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        *self.outbound.lock().expect("Claude outbound mutex") = Some(outbound_tx);
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(20)));

        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = outbound_rx.recv().await {
                let Ok(mut encoded) = serde_json::to_vec(&message) else {
                    continue;
                };
                encoded.push(b'\n');
                if stdin.write_all(&encoded).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

        let reader_pending = Arc::clone(&self.pending);
        let reader_events = self.events.clone();
        let reader_tail = Arc::clone(&stderr_tail);
        let reader_outbound = Arc::clone(&self.outbound);
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) => {
                            route_message(message, &reader_pending, &reader_events).await
                        }
                        Err(error) => {
                            let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                                "Claude SDK JSON 해석 실패: {error}"
                            )));
                            // A malformed question used to be discarded here, leaving
                            // Claude blocked forever while the user never saw a dialog.
                            // The bridge keeps the question and retries when it receives
                            // this explicit delivery failure.
                            if let Some(id) = recover_user_input_request_id(&line)
                                && let Some(outbound) = reader_outbound
                                    .lock()
                                    .expect("Claude outbound mutex")
                                    .as_ref()
                                    .cloned()
                            {
                                let _ = outbound.send(json!({
                                    "id": id,
                                    "error": {
                                        "code": -32700,
                                        "message": "사용자 입력 화면에 전달하지 못했습니다. 다시 시도합니다."
                                    }
                                }));
                            }
                        }
                    },
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                            "Claude SDK 출력 읽기 실패: {error}"
                        )));
                        break;
                    }
                }
            }
            reader_outbound
                .lock()
                .expect("Claude outbound mutex")
                .take();
            let tail = reader_tail.lock().await;
            let detail = if tail.is_empty() {
                "Claude Agent SDK 브리지 연결이 종료되었습니다.".to_owned()
            } else {
                format!(
                    "Claude Agent SDK 브리지 연결이 종료되었습니다.\n{}",
                    tail.iter().cloned().collect::<Vec<_>>().join("\n")
                )
            };
            drop(tail);
            for (_, sender) in reader_pending.lock().await.drain() {
                let _ = sender.send(Err(detail.clone()));
            }
            let _ = reader_events.send(ServerEvent::Closed(detail));
        });

        let stderr_buffer = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = stderr_buffer.lock().await;
                if tail.len() == 20 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });

        *self.process.lock().await = Some(ClaudeProcess {
            child,
            _job: job,
            writer_task,
            reader_task,
            stderr_task,
        });
        Ok(())
    }

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .lock()
            .expect("Claude outbound mutex")
            .as_ref()
            .ok_or_else(|| anyhow!("Claude SDK 브리지가 시작되지 않았거나 종료되었습니다."))?
            .send(message)
            .map_err(|_| anyhow!("Claude SDK 브리지에 메시지를 보낼 수 없습니다."))
    }
}

pub struct ClaudeServer {
    client: ClaudeClient,
    events: mpsc::UnboundedReceiver<ServerEvent>,
}

impl ClaudeServer {
    pub fn new(node_path: &Path, claude_path: &Path, cwd: &Path) -> Result<Self> {
        let bridge_path = resolve_bridge_path(cwd)?;
        let (event_tx, events) = mpsc::unbounded_channel();
        let client = ClaudeClient {
            outbound: Arc::new(StdMutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            process: Arc::new(Mutex::new(None)),
            start_lock: Arc::new(Mutex::new(())),
            events: event_tx,
            node_path: resolve_command(node_path),
            claude_path: resolve_command(claude_path),
            bridge_path,
            cwd: cwd.to_path_buf(),
        };
        Ok(Self { client, events })
    }

    pub async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        if let Some(object) = params.as_object_mut() {
            object.insert(
                "claudePath".to_owned(),
                json!(self.client.claude_path.to_string_lossy()),
            );
        }
        self.client.request(method, params).await
    }

    pub fn client(&self) -> ClaudeClient {
        self.client.clone()
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

    pub async fn shutdown(self) {
        if self
            .client
            .outbound
            .lock()
            .expect("Claude outbound mutex")
            .is_none()
        {
            return;
        }
        let _ = self.client.send(json!({
            "id": self.client.next_id.fetch_add(1, Ordering::Relaxed),
            "method": "shutdown",
            "params": {}
        }));
        self.client
            .outbound
            .lock()
            .expect("Claude outbound mutex")
            .take();
        let Some(mut process) = self.client.process.lock().await.take() else {
            return;
        };
        let _ = timeout(Duration::from_secs(2), &mut process.writer_task).await;
        if timeout(Duration::from_secs(3), process.child.wait())
            .await
            .is_err()
        {
            let _ = process.child.kill().await;
            let _ = process.child.wait().await;
        }
        process.reader_task.abort();
        process.stderr_task.abort();
    }
}

pub fn is_claude_model(model: &str) -> bool {
    model.starts_with("claude:")
        || matches!(
            model.to_ascii_lowercase().as_str(),
            "claude" | "sonnet" | "opus" | "fable" | "haiku"
        )
}

pub fn is_claude_thread(id: &str) -> bool {
    id.starts_with("claude:")
}

pub fn raw_thread_id(id: &str) -> &str {
    id.strip_prefix("claude:").unwrap_or(id)
}

pub fn visible_thread_id(id: &str) -> String {
    if is_claude_thread(id) {
        id.to_owned()
    } else {
        format!("claude:{id}")
    }
}

pub fn is_claude_request_id(id: &Value) -> bool {
    id.get("backend").and_then(Value::as_str) == Some("claude")
}

pub fn model_catalog() -> Value {
    let efforts = || {
        json!([
            { "reasoningEffort": "low" },
            { "reasoningEffort": "medium" },
            { "reasoningEffort": "high" },
            { "reasoningEffort": "xhigh" },
            { "reasoningEffort": "max" }
        ])
    };
    // Safeguard fallback only; `hidden` keeps it out of /model.
    let mut previous_opus =
        claude_model("claude:claude-opus-5", "Claude Opus 5", efforts(), false);
    previous_opus["hidden"] = json!(true);
    json!({
        "data": [
            claude_model("claude:claude-fable-5-1", "Claude Fable 5.1", efforts(), false),
            claude_model("claude:fable", "Claude Fable 5", efforts(), false),
            claude_model("claude:opus", "Claude Opus 5.5", efforts(), false),
            previous_opus,
            claude_model("claude:sonnet", "Claude Sonnet 5", efforts(), true),
            claude_model("claude:haiku", "Claude Haiku 4.5", json!([]), false)
        ]
    })
}

fn claude_model(id: &str, display_name: &str, efforts: Value, is_default: bool) -> Value {
    let default_effort = efforts
        .as_array()
        .filter(|efforts| !efforts.is_empty())
        .map(|_| "high")
        .unwrap_or("");
    json!({
        "id": id,
        "model": id,
        "displayName": display_name,
        "defaultReasoningEffort": default_effort,
        "supportedReasoningEfforts": efforts,
        "isDefault": is_default,
        "supportsAutoMode": !id.ends_with("haiku"),
        "contextWindow": 200_000
    })
}

fn claude_request_id(id: &Value) -> Result<&Value> {
    id.get("id")
        .filter(|_| is_claude_request_id(id))
        .context("Claude 사용자 입력 요청 id가 올바르지 않습니다.")
}

/// The bridge writes host request ids before the request payload. When a
/// malformed payload cannot be decoded as JSON, recover only the generated
/// ASCII id for a user-input request so the bridge can retry it safely.
fn recover_user_input_request_id(line: &str) -> Option<&str> {
    let method = r#""method":"item/tool/requestUserInput""#;
    let prefix = line.split_once(method)?.0;
    let marker = r#""id":""#;
    let (_, after_id) = prefix.split_once(marker)?;
    let id = after_id.split_once('"')?.0;
    (!id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(id)
}

async fn route_message(
    message: Value,
    pending: &PendingMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && (message.get("result").is_some() || message.get("error").is_some())
    {
        if let Some(sender) = pending.lock().await.remove(&id) {
            let response = match message.get("error") {
                Some(error) => Err(format_rpc_error(error)),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(response);
        }
        return;
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let _ = events.send(ServerEvent::ProtocolWarning(
            "method 없는 Claude SDK 메시지를 무시했습니다.".to_owned(),
        ));
        return;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match message.get("id") {
        Some(id) => {
            let _ = events.send(ServerEvent::Request {
                id: json!({ "backend": "claude", "id": id }),
                method: method.to_owned(),
                params,
            });
        }
        None => {
            let _ = events.send(ServerEvent::Notification {
                method: method.to_owned(),
                params,
            });
        }
    }
}

fn format_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("알 수 없는 Claude SDK 오류");
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} ({code})"),
        None => message.to_owned(),
    }
}

pub(crate) fn resolve_bridge_path(cwd: &Path) -> Result<PathBuf> {
    if let Some(path) = env::var_os("DEVEZ_VIBE_CLAUDE_BRIDGE").map(PathBuf::from)
        && path.is_file()
    {
        return Ok(path);
    }
    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe()
        && let Some(package_root) = executable.parent().and_then(Path::parent)
    {
        candidates.push(
            package_root
                .join("bridge")
                .join("claude-agent-sdk-bridge.mjs"),
        );
    }
    candidates.push(
        cwd.join("npm")
            .join("bridge")
            .join("claude-agent-sdk-bridge.mjs"),
    );
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("npm")
            .join("bridge")
            .join("claude-agent-sdk-bridge.mjs"),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .context("Claude Agent SDK 브리지 파일을 찾을 수 없습니다.")
}

fn resolve_command(command: &Path) -> PathBuf {
    if command.components().count() > 1 || command.exists() {
        return command.to_path_buf();
    }
    let Some(path) = env::var_os("PATH") else {
        return command.to_path_buf();
    };
    #[cfg(windows)]
    let extensions = [".exe", ".cmd", ".bat", ".com"];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_identifiers_are_namespaced() {
        assert!(is_claude_model("claude:sonnet"));
        assert!(is_claude_model("sonnet"));
        assert_eq!(visible_thread_id("123"), "claude:123");
        assert_eq!(raw_thread_id("claude:123"), "123");
    }

    #[test]
    fn bridge_forwards_claude_permission_prompts_instead_of_auto_allowing_them() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("Forward every such request to the host"));
        assert!(!bridge.contains("if (!permission.matchedAskRule && !planApproval)"));
    }

    #[test]
    fn bridge_verifies_auto_without_permission_bypass() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("const PREFERRED_PERMISSION_MODE = \"auto\""));
        assert!(!bridge.contains("allowDangerouslySkipPermissions: true"));
        assert!(bridge.contains("await session.query.setPermissionMode(PREFERRED_PERMISSION_MODE)"));
        assert_eq!(bridge.matches("await applyPermissionMode(session);").count(), 1);
        assert!(!bridge.contains("claude/permissionMode/rejected"));
    }

    #[tokio::test]
    #[ignore = "실제 Claude SDK 초기화와 로그인 필요"]
    async fn live_claude_auto_permission_mode() {
        let cwd = std::env::current_dir().unwrap();
        let server = ClaudeServer::new(Path::new("node"), Path::new("claude"), &cwd).unwrap();
        let result = async {
            let opened = timeout(Duration::from_secs(60), server.request("session/start", json!({
                "cwd": std::env::temp_dir(), "model": "claude:sonnet", "permissionMode": "bypassPermissions"
            }))).await.unwrap()?;
            let id = opened["id"].clone();
            let actual = server.request("session/permissionMode", json!({"sessionId": id})).await?;
            assert_eq!(actual["permissionMode"], "auto");
            server.request("session/resume", json!({"sessionId": id, "model": "claude:sonnet"})).await?;
            let resumed = server.request("session/permissionMode", json!({"sessionId": id})).await?;
            assert_eq!(resumed["permissionMode"], "auto");
            Ok::<(), anyhow::Error>(())
        }.await;
        server.shutdown().await;
        result.unwrap();
    }

    /// 패널에서 끌어낸 인용은 사용자 글자와 별개의 조각으로 나간다. 브리지가 그
    /// 조각을 따로 실어 보내는지, 모델이 그 안의 값을 읽는지 실제로 확인한다.
    #[tokio::test]
    #[ignore = "실제 Claude SDK 초기화와 로그인 필요"]
    async fn live_claude_carries_a_diff_selection_beside_the_prompt() {
        use crossterm::event::{KeyCode, KeyEvent};
        use std::time::{SystemTime, UNIX_EPOCH};

        let cwd = std::env::current_dir().unwrap();
        let mut server = ClaudeServer::new(Path::new("node"), Path::new("claude"), &cwd).unwrap();
        let result = async {
            let opened = timeout(
                Duration::from_secs(60),
                server.request(
                    "session/start",
                    json!({ "cwd": std::env::temp_dir(), "model": "claude:sonnet" }),
                ),
            )
            .await
            .unwrap()?;
            let id = opened["id"].clone();
            let mut state = crate::state::AppState::new(
                visible_thread_id(id.as_str().unwrap()),
                std::env::temp_dir().to_string_lossy().into(),
                "시험".into(),
                Vec::new(),
                "claude:sonnet",
                None,
            );
            let token = format!(
                "ZQX{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            assert!(state.set_diff_reference(format!("let rune = \"{token}\";")));
            state.handle_paste(
                "인용된 줄에 들어 있는 값을 그대로 한 번만 적으세요. 도구는 쓰지 마세요.",
            );
            let crate::state::Action::Submit(text) =
                state.handle_key(KeyEvent::from(KeyCode::Enter))
            else {
                panic!("프롬프트가 나가야 한다");
            };
            let input = state.turn_input(text);
            assert_eq!(input.len(), 2, "인용이 별도 조각으로 서지 않음: {input:?}");
            server
                .request(
                    "session/prompt",
                    json!({ "sessionId": id, "input": input, "model": "claude:sonnet" }),
                )
                .await?;
            let mut answer = String::new();
            loop {
                let event = timeout(Duration::from_secs(180), server.next_event())
                    .await
                    .expect("실제 모델 응답 시간 초과")
                    .expect("Connection closed");
                let ServerEvent::Notification { method, params } = event else {
                    continue;
                };
                if method == "item/completed"
                    && params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage")
                {
                    answer.push_str(params.pointer("/item/text").and_then(Value::as_str).unwrap_or_default());
                }
                if method == "turn/completed" {
                    break;
                }
            }
            assert!(answer.contains(&token), "모델이 인용된 값을 읽지 못함: {answer}");
            println!("검증 통과: claude:sonnet 인용 조각 전송과 모델 인식");
            Ok::<(), anyhow::Error>(())
        }
        .await;
        server.shutdown().await;
        result.unwrap();
    }

    /// 끊긴 브리지 자리가 남아 있어도 다음 요청이 새로 띄우는지 본다. 시작에
    /// 실패한 오류가 나오면 재시작을 요구하는 대신 재기동을 시도한 것이다.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_dead_bridge_starts_again_instead_of_asking_for_a_restart() {
        let client = ClaudeClient {
            outbound: Arc::new(StdMutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            process: Arc::new(Mutex::new(None)),
            start_lock: Arc::new(Mutex::new(())),
            events: mpsc::unbounded_channel().0,
            node_path: PathBuf::from("devez-vibe-없는-node"),
            claude_path: PathBuf::from("claude"),
            bridge_path: PathBuf::from("bridge.mjs"),
            cwd: env::temp_dir(),
        };
        let mut child = Command::new("cmd").args(["/c", "exit"]).spawn().unwrap();
        let _ = child.wait().await;
        *client.process.lock().await = Some(ClaudeProcess {
            child,
            _job: None,
            writer_task: tokio::spawn(async {}),
            reader_task: tokio::spawn(async {}),
            stderr_task: tokio::spawn(async {}),
        });

        let error = client.ensure_started().await.unwrap_err().to_string();

        assert!(error.contains("시작하지 못했습니다"), "재기동을 시도해야 한다: {error}");
        assert!(client.process.lock().await.is_none(), "끊긴 자리는 치운다");
    }

    /// 브리지가 살아 있는 채로 응답만 멈추면 요청을 끊고 알린다. 시간을 멈춘
    /// 검사라 실제로 상한만큼 기다리지 않는다.
    #[tokio::test(start_paused = true)]
    async fn a_silent_bridge_ends_the_request_instead_of_hanging() {
        let (outbound, _inbox) = mpsc::unbounded_channel();
        let client = ClaudeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            process: Arc::new(Mutex::new(None)),
            start_lock: Arc::new(Mutex::new(())),
            events: mpsc::unbounded_channel().0,
            node_path: PathBuf::from("node"),
            claude_path: PathBuf::from("claude"),
            bridge_path: PathBuf::from("bridge.mjs"),
            cwd: env::temp_dir(),
        };

        let error = client
            .request("session/start", json!({}))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("응답하지 않아"), "{error}");
        assert!(
            client.pending.lock().await.is_empty(),
            "끊은 요청은 대기 목록에 남기지 않는다"
        );
    }

    /// 계정·플러그인 조회가 CLI에서 멈추면 브리지 요청도 끝나지 않는다.
    #[test]
    fn bridge_bounds_claude_cli_lookups() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("const CLAUDE_COMMAND_TIMEOUT = 60_000;"));
        assert!(bridge.contains("}, deadlineMs);"));
        // 설치와 마켓플레이스 갱신은 저장소를 받아 오므로 조회 상한으로 끊지 않는다.
        assert!(bridge.contains("const CLAUDE_INSTALL_TIMEOUT = 480_000;"));
        assert!(bridge.contains("await runClaudeCommand(params, [\"plugin\", \"install\", id, \"--scope\", \"user\"], CLAUDE_INSTALL_TIMEOUT);"));
        assert!(bridge.contains("await runCommandTimeoutSelfTest();"));
    }

    /// 합류할 턴이 사라진 이어 말하기는 새 턴이 된다. 그때 역할의 쓰기 제한과
    /// 고른 추론 수준이 조용히 풀리면 읽기 전용 역할이 저장소를 고칠 수 있다.
    #[test]
    fn a_reopened_turn_keeps_the_role_limit_and_effort() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains(r#"if ("toolPolicy" in params) {"#));
        assert!(bridge.contains("params.effort ?? session.effort"));
    }

    /// 다시 뜬 브리지가 예전 질문과 같은 번호를 쓰면 새 질문이 사라진다.
    #[test]
    fn bridge_question_ids_differ_between_runs() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("const HOST_REQUEST_RUN ="));
        assert!(bridge.contains("`claude-host-${HOST_REQUEST_RUN}-${nextHostRequest++}`"));
    }

    #[test]
    fn malformed_user_input_request_recovers_only_the_safe_bridge_id() {
        let line = r#"{"id":"claude-host-42","method":"item/tool/requestUserInput","params":{"payload":"\u12"}}"#;
        assert_eq!(recover_user_input_request_id(line), Some("claude-host-42"));
        // 실행마다 붙는 접두도 안전한 문자만 쓴다.
        let with_run = r#"{"id":"claude-host-k3f9a1-42","method":"item/tool/requestUserInput"}"#;
        assert_eq!(
            recover_user_input_request_id(with_run),
            Some("claude-host-k3f9a1-42")
        );
        assert_eq!(
            recover_user_input_request_id(r#"{"id":"42","method":"turn/start"}"#),
            None
        );
        assert_eq!(
            recover_user_input_request_id(
                r#"{"id":"not safe!","method":"item/tool/requestUserInput"}"#
            ),
            None
        );
    }

    #[test]
    fn model_catalog_uses_existing_model_shape() {
        let catalog = model_catalog();
        let models = catalog.get("data").and_then(Value::as_array).unwrap();
        assert_eq!(models.len(), 6);
        assert_eq!(
            models
                .iter()
                .filter_map(|model| model.get("model").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [
                "claude:claude-fable-5-1",
                "claude:fable",
                "claude:opus",
                "claude:claude-opus-5",
                "claude:sonnet",
                "claude:haiku"
            ]
        );
        assert!(
            models
                .iter()
                .all(|model| model.get("model").and_then(Value::as_str) != Some("claude:default"))
        );
        assert_eq!(
            models
                .iter()
                .filter_map(|model| model.get("displayName").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [
                "Claude Fable 5.1",
                "Claude Fable 5",
                "Claude Opus 5.5",
                "Claude Opus 5",
                "Claude Sonnet 5",
                "Claude Haiku 4.5"
            ]
        );
        assert_eq!(
            models
                .iter()
                .find(|model| model.get("isDefault").and_then(Value::as_bool) == Some(true))
                .and_then(|model| model.get("model"))
                .and_then(Value::as_str),
            Some("claude:sonnet")
        );
        let haiku = models
            .iter()
            .find(|model| model.get("model").and_then(Value::as_str) == Some("claude:haiku"))
            .unwrap();
        assert_eq!(haiku.pointer("/supportedReasoningEfforts/0"), None);
    }

    #[test]
    fn bridge_uses_the_latest_assistant_request_for_context_usage() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(
            bridge.contains("session.lastContextUsage = tokenBreakdown(message.message?.usage)")
        );
        assert!(bridge.contains("last: session.lastContextUsage"));
        assert!(!bridge.contains("last: totals"));
    }

    #[test]
    fn bridge_reports_context_usage_as_soon_as_compaction_ends() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("noteCompactBoundary(session, message.compact_metadata)"));
        assert!(bridge.contains("const post = Number(metadata?.post_tokens);"));
        assert!(bridge.contains("function noteCompactBoundary(session, metadata)"));
    }

    /// The SDK's own reader walks the parentUuid chain, which a compaction
    /// boundary breaks, so a resumed session would lose everything said before
    /// the last compact. History has to come from the transcript file itself.
    #[test]
    fn bridge_restores_history_from_the_transcript_file_across_compactions() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("function transcriptEntries(raw)"));
        assert!(bridge.contains("async function sessionMessages(id, cwd)"));
        assert!(bridge.contains("const messages = await sessionMessages(id, params.cwd);"));
        assert!(bridge.contains("Claude compacted transcript self-test failed"));
    }

    /// A resumed transcript may name a model the general catalog no longer
    /// lists (Fable 5 after Fable 5.1 shipped). Left alone, the host cannot
    /// select it and the next prompt moves the session onto the host default.
    #[test]
    fn bridge_keeps_a_resumed_session_in_its_model_family() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("function familyCapabilities(models, model)"));
        assert!(bridge.contains("function cachedCatalogValues(params)"));
        assert!(bridge.contains("await agentQuery.setModel(successor.value);"));
        assert!(bridge.contains("Claude model successor self-test failed"));
    }

    /// On Windows the SDK bundle stays the default, but an installed Claude Code
    /// that is a real executable and strictly newer takes over, so updating
    /// Claude Code alone surfaces new models.
    #[test]
    fn bridge_prefers_a_newer_installed_claude_code_on_windows() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("function prefersInstalledCli(executable, installedVersion, bundledVersion)"));
        assert!(bridge.contains("async function ensureClaudeExecutableChoice(params)"));
        assert!(bridge.contains("if (externalCliChoices.get(executable) === true) options.pathToClaudeCodeExecutable = executable;"));
        assert!(bridge.contains("Claude installed CLI preference self-test failed"));
    }

    #[test]
    fn bridge_reads_the_current_claude_account_without_a_session() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("method === \"account/read\""));
        assert!(bridge.contains("[\"auth\", \"status\", \"--json\"]"));
    }

    #[test]
    fn bridge_filters_local_commands_and_restores_each_turn_model() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("isInternalHistoryText(message, userText)"));
        assert!(bridge.contains("\"command-name\""));
        assert!(bridge.contains("\"local-command-caveat\""));
        assert!(bridge.contains("\"bash-input\""));
        assert!(bridge.contains("\"task-notification\""));
        assert!(bridge.contains("\"system-reminder\""));
        assert!(bridge.contains("\"command-message\""));
        assert!(bridge.contains("stripInternalTags(stripHandoff("));
        assert!(bridge.contains("isCompactSummary(message, text)"));
        assert!(bridge.contains("message.isCompactSummary === true"));
        assert!(bridge.contains("[Request interrupted by user\""));
        assert!(bridge.contains("message.message?.model === \"<synthetic>\""));
        assert!(bridge.contains("prompt.model = turn.model"));
    }

    #[test]
    fn bridge_restores_tasks_and_only_resets_an_explicit_new_plan() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("function historyState(messages)"));
        assert!(bridge.contains("session.tasks = historyState(messages).tasks;"));
        assert!(bridge.contains("if (numberedTaskIndex(subject) === 1) tasks.clear();"));
        assert!(bridge.contains(
            "applyTaskUpdate(session.tasks, input, turnId, () => emitPlan(session), Date.now());"
        ));
        // A restored plan totals to zero unless the step times ride with it.
        assert!(bridge.contains("elapsedMs: task.elapsedMs ?? null,"));
        assert!(bridge.contains("function messageTime(message)"));
        assert!(bridge.contains("session.tasks = latestTaskPlan(session.tasks);"));
        assert!(
            bridge.contains(
                "every skipped predecessor and the target itself pass through in_progress"
            )
        );
        // 정상 종료 턴에서만 남은 진행 중 작업을 완료로 맞춘다.
        assert!(bridge.contains("if (turn.status === \"completed\") completeLingeringTasks(session, Date.now());"));
    }

    #[test]
    fn bridge_enables_latest_claude_task_and_interrupt_contracts() {
        let package: Value =
            serde_json::from_str(include_str!("../npm/package.json")).expect("npm package");
        let lock: Value =
            serde_json::from_str(include_str!("../npm/package-lock.json")).expect("npm lock");
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert_eq!(
            package
                .pointer("/dependencies/@anthropic-ai~1claude-agent-sdk")
                .and_then(Value::as_str),
            Some("0.3.280")
        );
        assert_eq!(
            lock.pointer("/packages//dependencies/@anthropic-ai~1claude-agent-sdk")
                .and_then(Value::as_str),
            Some("0.3.280")
        );
        assert!(bridge.contains(
            "const CLAUDE_TASK_TOOLS = [\"TaskCreate\", \"TaskGet\", \"TaskUpdate\", \"TaskList\"]"
        ));
        assert!(bridge.contains("allowedTools: [...CLAUDE_TASK_TOOLS]"));
        assert!(bridge.contains("perTaskStopAffordance: true"));
        // A read-only role turn is enforced by a PreToolUse hook that reads the
        // policy the host sends with each prompt, whatever the permission mode.
        assert!(bridge.contains("PreToolUse: [{ hooks: [(input) => toolPolicyHook(sessionId, input)] }]"));
        assert!(bridge.contains("session.toolPolicy = "));
        assert!(bridge.contains("permissionDecision: \"deny\""));
        assert!(bridge.contains("if (CLAUDE_TASK_TOOLS.includes(name))"));
        assert!(bridge.contains("[\"TaskGet\", \"TaskList\", \"AskUserQuestion\"]"));
    }

    #[test]
    fn bridge_renumbers_tasks_from_one_and_never_sends_an_empty_plan() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        // 모델이 붙인 누적 번호를 떼고 지금 보여줄 목록 기준으로 다시 매긴다.
        assert!(bridge.contains(".replace(/^\\d+[.)]\\s*/, \"\")"));
        assert!(bridge.contains("return `${index + 1}. ${text || \"작업\"}`;"));
        // 계획 카드가 이유 없이 사라지지 않도록 빈 목록은 알리지 않고 직전 목록을 지킨다.
        assert!(bridge.contains("if (session.tasks.size === 0) return;"));
    }

    #[test]
    fn bridge_tracks_foreground_and_background_subagent_lifecycles() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(
            bridge.contains("if (SUBAGENT_TOOLS.includes(name)) startSubagent(session, block);")
        );
        assert!(bridge.contains("recordSubagentMessage(session, message)"));
        assert!(bridge.contains("recordSubagentResult(session, message)"));
        assert!(bridge.contains("notify(\"turn/subagent/line\""));
        // The artifact row under the status line listens for this method name.
        assert!(bridge.contains("notify(\"turn/artifact/published\""));
        // A resume replays the transcript's published pages into that row.
        assert!(bridge.contains("restoreArtifacts(id, messages)"));
        assert!(bridge.contains("findSubagent(session, message.parent_tool_use_id)"));
        assert!(bridge.contains("keepBackgroundSubagent(session, block.tool_use_id"));
        assert!(bridge.contains("result?.status === \"async_launched\""));
        assert!(bridge.contains("message.origin?.kind !== \"task-notification\""));
        assert!(bridge.contains("finishNotifiedSubagents(session, notifications)"));
        assert!(bridge.contains("resumeBackgroundSubagent(session, block.tool_use_id"));
        assert!(bridge.contains("processSubagentSystemMessage(session, message)"));
        assert!(bridge.contains("message.subtype === \"background_tasks_changed\""));
        assert!(bridge.contains("message.subtype === \"task_notification\""));
        assert!(bridge.contains("message.ambient === true"));
        assert!(bridge.contains("session.ambientSubagentTasks"));
        assert!(bridge.contains("clearForegroundSubagents(session)"));
        assert!(bridge.contains("if (!session.turn) {"));
        assert!(bridge.contains("beginUntrackedTurn(session, message)"));
        assert!(bridge.contains("await consumeMessage(session, message)"));
        assert!(bridge.contains("session.automaticTurnsPending > 0"));
        assert!(bridge.contains("if (message.isReplay === true) return;"));
        assert!(
            bridge.contains("message.type !== \"stream_event\" && message.type !== \"assistant\"")
        );
        assert!(bridge.contains("notify(\"turn/subagents/updated\""));
    }

    /// 교차 제공자 검증자는 Agent 도구가 아니라 셸로 뜬다. 명령만 보이면 몇 개가
    /// 돌고 언제 끝나는지 알 수 없으므로 같은 서브에이전트 목록에 올린다.
    #[test]
    fn bridge_lists_shell_launched_delegated_agents_as_subagents() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("else if (name === \"Bash\") startDelegatedSubagent(session, block);"));
        assert!(bridge.contains("const DELEGATED_AGENT_PATTERN = /\\bcodex\\s+exec\\b/;"));
        // 표시 이름은 실제로 지정된 모델을 따른다.
        assert!(bridge.contains("name: firstLine(delegatedAgentModel(command) || \"codex\", 40)"));
        // 위임 실행이 아닌 일반 명령은 목록에 올리지 않는다.
        assert!(bridge.contains("if (!DELEGATED_AGENT_PATTERN.test(command)) return;"));
    }

    /// 백그라운드 목록은 하위 에이전트만의 것이 아니다. 백그라운드로 돌린 명령을
    /// 종류로 걸러 내면 실행 중인 일이 화면에서 통째로 사라진다.
    #[test]
    fn bridge_lists_background_commands_next_to_subagents() {
        let bridge = include_str!("../npm/bridge/claude-agent-sdk-bridge.mjs");

        assert!(bridge.contains("name: known?.name || backgroundTaskName(task?.task_type)"));
        assert!(!bridge.contains("if (!running && !known && !isSubagentTaskType(task?.task_type))"));
        assert!(bridge.contains("function backgroundTaskName(taskType)"));
    }
}
