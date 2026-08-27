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
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
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
        match response_rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => bail!("{method}: {error}"),
            Err(_) => bail!("{method}: Claude SDK 응답 채널이 종료되었습니다."),
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
        if self.process.lock().await.is_some() {
            bail!("Claude SDK 브리지 연결이 종료되었습니다. Devez Vibe를 다시 시작하세요.");
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
    json!({
        "data": [
            claude_model("claude:fable", "Claude Fable 5", efforts(), false),
            claude_model("claude:opus", "Claude Opus 5", efforts(), false),
            claude_model(
                "claude:claude-opus-4-8",
                "Claude Opus 4.8",
                efforts(),
                false,
            ),
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

fn resolve_bridge_path(cwd: &Path) -> Result<PathBuf> {
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

        assert!(bridge.contains("\"dontAsk\""));
        assert!(bridge.contains("Forward every such request to the host"));
        assert!(!bridge.contains("if (!permission.matchedAskRule && !planApproval)"));
    }

    #[test]
    fn malformed_user_input_request_recovers_only_the_safe_bridge_id() {
        let line = r#"{"id":"claude-host-42","method":"item/tool/requestUserInput","params":{"payload":"\u12"}}"#;
        assert_eq!(recover_user_input_request_id(line), Some("claude-host-42"));
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
        assert_eq!(models.len(), 5);
        assert_eq!(
            models
                .iter()
                .filter_map(|model| model.get("model").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            [
                "claude:fable",
                "claude:opus",
                "claude:claude-opus-4-8",
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
                "Claude Fable 5",
                "Claude Opus 5",
                "Claude Opus 4.8",
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
            Some("0.3.247")
        );
        assert_eq!(
            lock.pointer("/packages//dependencies/@anthropic-ai~1claude-agent-sdk")
                .and_then(Value::as_str),
            Some("0.3.247")
        );
        assert!(bridge.contains(
            "const CLAUDE_TASK_TOOLS = [\"TaskCreate\", \"TaskGet\", \"TaskUpdate\", \"TaskList\"]"
        ));
        assert!(bridge.contains("allowedTools: [...CLAUDE_TASK_TOOLS]"));
        assert!(bridge.contains("perTaskStopAffordance: true"));
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
        assert!(bridge.contains("BACKGROUND_SUBAGENT_LEASE_MS"));
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
