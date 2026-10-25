use std::{
    collections::{HashMap, VecDeque},
    env,
    path::Path,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

use crate::language::tr;

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type PendingMap = Arc<StdMutex<HashMap<u64, PendingResponse>>>;

struct PendingRequest<'a> {
    pending: &'a PendingMap,
    id: u64,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.pending.lock().expect("pending mutex").remove(&self.id);
    }
}

/// app-server가 죽지 않고 응답만 멈추면 요청이 영원히 매달려 스피너만 남는다.
/// 세션 시작이나 큰 롤아웃 복원도 이보다 오래 걸리지는 않으므로, 넘기면
/// 멈춘 것으로 보고 사용자에게 알린다.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
// A stop request must not hold Escape hostage to the normal 10-minute RPC budget.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);

/// The cloneable half of the app-server connection. Background work may issue
/// requests through it while [`AppServer`] remains the sole reader of events.
#[derive(Clone)]
pub struct AppServerClient {
    outbound: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    closed: watch::Sender<bool>,
}

impl AppServerClient {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() { bail!("The app-server connection has already closed."); }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().expect("pending mutex").insert(id, response_tx);
        // A caller's timeout or task cancellation also drops the response slot.
        let _pending_request = PendingRequest { pending: &self.pending, id };

        let message = json!({
            "id": id,
            "method": method,
            "params": params
        });

        self.send(message)?;

        let deadline = if method == "turn/interrupt" { INTERRUPT_TIMEOUT } else { REQUEST_TIMEOUT };
        let response = tokio::select! {
            biased;
            _ = closed.changed() => {
                bail!(tr("app-server 연결이 종료되었습니다.", "The app-server connection closed."));
            }
            response = timeout(deadline, response_rx) => response,
        };
        match response {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("{method}: {error}"),
            Ok(Err(_)) => bail!(tr(
                format!("{method}: app-server 응답 채널이 종료되었습니다."),
                format!("{method}: the app-server response channel closed."),
            )),
            Err(_) => {
                if method == "turn/interrupt" {
                    bail!(tr(
                        "Codex가 5초 안에 중단 요청에 응답하지 않았습니다. Esc로 다시 시도할 수 있습니다.",
                        "Codex did not acknowledge the stop request within 5 seconds. Press Escape to retry.",
                    ));
                }
                let minutes = REQUEST_TIMEOUT.as_secs() / 60;
                bail!(tr(
                    format!("{method}: Codex app-server가 {minutes}분 동안 응답하지 않아 요청을 중단했습니다."),
                    format!("{method}: stopped the request because the Codex app-server didn't answer for {minutes} minutes."),
                ))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let message = match params {
            Some(params) => json!({ "method": method, "params": params }),
            None => json!({ "method": method }),
        };
        self.send(message)
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({ "id": id, "result": result }))
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        self.send(json!({
            "id": id,
            "error": {
                "code": code,
                "message": message
            }
        }))
    }

    fn close(&self) {
        self.outbound.lock().expect("outbound mutex").take();
        self.closed.send_replace(true);
    }

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .lock()
            .expect("outbound mutex")
            .as_ref()
            .ok_or_else(|| {
                anyhow!(tr("app-server 연결이 이미 종료되었습니다.", "The app-server connection has already closed."))
            })?
            .send(message)
            .map_err(|_| anyhow!(tr("app-server에 메시지를 보낼 수 없습니다.", "Can't send a message to the app-server.")))
    }
}

#[derive(Debug)]
pub enum ServerEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    ProtocolWarning(String),
    ProviderUnavailable {
        provider: String,
        message: String,
    },
    Closed(String),
}

pub struct AppServer {
    child: Child,
    /// app-server가 띄운 MCP 서버까지 함께 끝낸다.
    _job: Option<crate::child_process::BackendJob>,
    _cursor_restore: crate::cursor_recovery::RestoreOnDrop,
    cursor_enabled: Arc<AtomicBool>,
    computer_use: crate::computer_use::Monitor,
    client: AppServerClient,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    permission_events: mpsc::UnboundedSender<ServerEvent>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    config_path: Option<PathBuf>,
    computer_use_pipe: Mutex<Option<String>>,
}

impl AppServer {
    pub async fn spawn(codex_path: &Path, devezcode_room: Option<&str>) -> Result<Self> {
        let config_path = crate::state::codex_home().map(|home| home.join("config.toml"));
        let computer_use_pipe = config_path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|config| computer_use_pipe_config(&config));
        let cursor_enabled = Arc::new(AtomicBool::new(computer_use_pipe.is_some()));
        let cursor_restore = crate::cursor_recovery::RestoreOnDrop::new(Arc::clone(&cursor_enabled));
        if cursor_enabled.load(Ordering::Acquire) {
            let _ = crate::cursor_recovery::restore_if_idle();
        }
        let resolved_codex = resolve_command(codex_path);
        let mut command = codex_command(&resolved_codex);
        apply_originator_override(&mut command);
        apply_mcp_2026_protocol_override(&mut command);
        apply_update_plan_tool_override(&mut command);
        // Keep normal development tools available while exposing the native
        // question RPC. The host holds its response until the user answers.
        command
            .arg("-c")
            .arg("features.default_mode_request_user_input=true");
        apply_instant_interrupt_override(&mut command);
        apply_unstable_features_warning_override(&mut command);
        apply_devezcode_room_override(&mut command, devezcode_room);
        provision_devez_subagents();
        crate::child_process::isolate_backend(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                let codex = resolved_codex.display();
                tr(
                    format!("Codex app-server를 시작하지 못했습니다: {codex}"),
                    format!("Couldn't start the Codex app-server: {codex}"),
                )
            })?;

        let job = crate::child_process::adopt_backend(&child);
        let stdin = child.stdin.take().context(tr("app-server stdin 연결 실패", "app-server stdin connection failed"))?;
        let stdout = child.stdout.take().context(tr("app-server stdout 연결 실패", "app-server stdout connection failed"))?;
        let stderr = child.stderr.take().context(tr("app-server stderr 연결 실패", "app-server stderr connection failed"))?;

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, events) = mpsc::unbounded_channel::<ServerEvent>();
        let permission_events = event_tx.clone();
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(20)));

        let client = AppServerClient {
            outbound: Arc::new(StdMutex::new(Some(outbound_tx))),
            pending: Arc::clone(&pending),
            next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
        };
        let writer_task = tokio::spawn(write_messages(
            stdin, outbound_rx, client.clone(), event_tx.clone(),
        ));
        let computer_use = crate::computer_use::Monitor::start(
            client.clone(), event_tx.clone(), crate::state::codex_home(), Arc::clone(&cursor_enabled),
        );
        let computer_observer = computer_use.observer();
        let reader_client = client.clone();

        let reader_pending = Arc::clone(&pending);
        let reader_events = event_tx.clone();
        let reader_stderr = Arc::clone(&stderr_tail);
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&line) {
                            Ok(message) => {
                                computer_observer.observe(&message);
                                route_message(message, &reader_pending, &reader_events).await;
                            }
                            Err(error) => {
                                let _ = reader_events.send(ServerEvent::ProtocolWarning(tr(
                                    format!("app-server JSON 해석 실패: {error}"),
                                    format!("Couldn't parse app-server JSON: {error}"),
                                )));
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events.send(ServerEvent::ProtocolWarning(tr(
                            format!("app-server 출력 읽기 실패: {error}"),
                            format!("Couldn't read app-server output: {error}"),
                        )));
                        break;
                    }
                }
            }

            reader_client.close();
            computer_observer.closed();
            let tail = reader_stderr.lock().await;
            let closed = tr("app-server 연결이 종료되었습니다.", "The app-server connection closed.");
            let detail = if tail.is_empty() {
                closed.to_owned()
            } else {
                format!("{closed}\n{}", tail.iter().cloned().collect::<Vec<_>>().join("\n"))
            };
            drop(tail);

            let mut pending = reader_pending.lock().expect("pending mutex");
            for (_, sender) in pending.drain() {
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

        Ok(Self {
            child,
            _job: job,
            _cursor_restore: cursor_restore,
            cursor_enabled,
            computer_use,
            client,
            events,
            permission_events,
            writer_task,
            reader_task,
            stderr_task,
            config_path,
            computer_use_pipe: Mutex::new(computer_use_pipe),
        })
    }

    pub async fn initialize(&self) -> Result<Value> {
        let response = self.request("initialize", initialize_params()).await?;
        self.notify("initialized", None)?;
        Ok(response)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if matches!(method, "thread/start" | "thread/resume" | "thread/fork" | "turn/start") {
            if let Some(path) = &self.config_path {
                refresh_computer_use(&self.client, path, &self.computer_use_pipe).await?;
                if self.computer_use_pipe.lock().await.is_some() {
                    self.cursor_enabled.store(true, Ordering::Release);
                }
            }
            let thread = params.get("threadId").cloned();
            let cwd = params.get("cwd").and_then(Value::as_str).map(str::to_owned);
            if method == "turn/start" {
                self.computer_use.observer().begin_turn_request(thread.as_ref().and_then(Value::as_str), cwd.as_deref());
            }
            let response = request_with_permissions(&self.client, method, params).await;
            if method == "turn/start" {
                self.computer_use.observer().end_turn_request(thread.as_ref().and_then(Value::as_str), cwd.as_deref());
            }
            if response.is_err() {
                self.computer_use.observer().recover();
            }
            let (response, profile, lowered) = response?;
            self.computer_use.observer().register_thread(&response, cwd.as_deref());
            if method == "turn/start" {
                self.computer_use.observer().register_turn_response(thread.as_ref().and_then(Value::as_str), &response, cwd.as_deref());
            }
            if let Some(thread) = response.pointer("/thread/id").cloned().or(thread) {
                let _ = self.permission_events.send(ServerEvent::Notification {
                    method: "devez/permissions/updated".to_owned(),
                    params: json!({"threadId": thread, "profile": profile, "lowered": lowered}),
                });
            }
            return Ok(response);
        }
        let reloaded_config = if method == "config/mcpServer/reload" {
            self.config_path.as_deref()
                .and_then(|path| std::fs::read_to_string(path).ok())
        } else {
            None
        };
        let response = self.client.request(method, params).await;
        if method == "turn/interrupt" {
            self.computer_use.observer().recover();
        }
        let response = response?;
        if let Some(config) = reloaded_config {
            let pipe = computer_use_pipe_config(&config);
            if pipe.is_some() { self.cursor_enabled.store(true, Ordering::Release); }
            *self.computer_use_pipe.lock().await = pipe;
        }
        Ok(response)
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.client.notify(method, params)
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.client.respond(id, result)
    }

    pub fn client(&self) -> AppServerClient {
        self.client.clone()
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        self.client.respond_error(id, code, message)
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) {
        self.computer_use.stop();
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
        self._job = None;
        self.computer_use.finish_cleanup().await;
        if self.cursor_enabled.load(Ordering::Acquire) {
            let _ = crate::cursor_recovery::restore_after_turn().await;
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.client.close();
        self.computer_use.stop();
        self.writer_task.abort();
        self.reader_task.abort();
        self.stderr_task.abort();
        // Killing owned processes is asynchronous on Windows. The following
        // RestoreOnDrop field performs the bounded final cursor settle pass.
        self._job = None;
    }
}

async fn write_messages(
    mut stdin: impl AsyncWrite + Unpin,
    mut messages: mpsc::UnboundedReceiver<Value>,
    client: AppServerClient,
    events: mpsc::UnboundedSender<ServerEvent>,
) {
    while let Some(message) = messages.recv().await {
        let Ok(mut encoded) = serde_json::to_vec(&message) else { continue; };
        encoded.push(b'\n');
        let written = async {
            stdin.write_all(&encoded).await?;
            stdin.flush().await
        }.await;
        if let Err(error) = written {
            // stdout can stay open after stdin breaks. Wake pending requests
            // and let the backend release this runtime so the user can retry.
            client.close();
            let _ = events.send(ServerEvent::Closed(tr(
                format!("app-server 입력 쓰기 실패: {error}"),
                format!("Couldn't write app-server input: {error}"),
            )));
            break;
        }
    }
    let _ = stdin.shutdown().await;
}

/// The desktop supplies this pipe address through the Codex configuration.
/// Running MCP children retain the previous environment until they reload.
fn computer_use_pipe_config(config: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let line = line.split('#').next()?.trim();
        let (key, value) = line.split_once('=')?;
        let key = key.trim().trim_matches(['"', '\'']);
        matches!(
            key,
            "SKY_CUA_NATIVE_PIPE_DIRECTORY"
                | "mcp_servers.node_repl.env.SKY_CUA_NATIVE_PIPE_DIRECTORY"
        ).then(|| value.trim().to_owned())
    })
}

async fn refresh_computer_use(
    client: &AppServerClient,
    config_path: &Path,
    cached_pipe: &Mutex<Option<String>>,
) -> Result<()> {
    let Ok(config) = std::fs::read_to_string(config_path) else {
        return Ok(());
    };
    let pipe = computer_use_pipe_config(&config);
    let mut cached = cached_pipe.lock().await;
    if *cached != pipe {
        client.request("config/mcpServer/reload", json!({})).await?;
        *cached = pipe;
    }
    Ok(())
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "devez-vibe",
            "title": "Devez Vibe",
            "version": env!("CARGO_PKG_VERSION")
        },
        "capabilities": {
            "experimentalApi": true,
            "requestAttestation": false,
            // `mcpServerOpenaiFormElicitation` is the legacy alias an older
            // app-server still understands; `extensions` is how 0.147 wants the
            // same opt-in declared. Sending both keeps either version working.
            "mcpServerOpenaiFormElicitation": true,
            "extensions": {
                "openai/form": {}
            }
        }
    })
}

fn codex_command(resolved: &Path) -> Command {
    codex_invocation(resolved, &["app-server", "--listen", "stdio://"])
}

fn codex_invocation(resolved: &Path, args: &[&str]) -> Command {
    // Keep the selected launcher authoritative. The npm shim also selects the
    // platform package and prepares its environment; a guessed vendored binary
    // or adjacent codex.js can bypass that logic or a user's custom wrapper.
    // Rust handles .cmd/.bat with batch-aware escaping on Windows, including
    // arguments appended by the caller after this function returns.
    let mut command = Command::new(resolved);
    command.args(args);
    command
}

const TITLE_INSTRUCTIONS: &str =
    "Write a title for a conversation that begins with the user request below.
Not counting spaces, aim for about 20 bytes and never exceed 24, counting a Korean character as 2 bytes and an English letter or digit as 1 (about 10 Korean characters or 20 English letters).
No quotes or trailing punctuation. Reply with the title only. Do not run commands or use tools.";
/// Enough of a long first prompt to say what it is about.
const TITLE_PROMPT_CHARS: usize = 2_000;
/// Titles aim at about 20 bytes; this enforces the 24 the prompt allows.
const TITLE_BYTES: usize = 24;

/// What both runtimes are asked to title a new session from, in the language
/// `/language` shows.
pub(crate) fn title_request(prompt: &str) -> String {
    let request: String = prompt.chars().take(TITLE_PROMPT_CHARS).collect();
    let language = crate::language::tr("Korean", "English");
    format!(
        "{TITLE_INSTRUCTIONS}\nWrite the title in {language}.\n\n<request>\n{request}\n</request>"
    )
}

/// Codex's own clients name a thread from its first prompt; the app-server does
/// not, so the title comes from a one-off `codex exec`. It loads no user config,
/// rules or project instructions, saves no session, and runs outside the
/// DevezCode room so the host's Codex hooks never take it for the room's thread.
/// The name goes through `thread/name/set`, into the store Codex's own clients
/// write and DevezCode reads.
pub(crate) fn spawn_thread_title(
    codex_path: &Path,
    client: AppServerClient,
    thread_id: String,
    model: &str,
    prompt: &str,
) {
    let mut command = codex_invocation(
        &resolve_command(codex_path),
        &[
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "-c",
            "model_reasoning_effort=\"low\"",
            "-m",
            model,
        ],
    );
    command
        .arg("-C")
        .arg(env::temp_dir())
        .arg("-")
        .env_remove("DEVEZCODE_ROOM_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::child_process::isolate_backend(&mut command);
    let input = title_request(prompt);
    tokio::spawn(async move {
        let Ok(mut child) = command.spawn() else {
            return;
        };
        let _job = crate::child_process::adopt_backend(&child);
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        let Ok(Ok(output)) = timeout(Duration::from_secs(120), child.wait_with_output()).await
        else {
            return;
        };
        let Some(title) = output
            .status
            .success()
            .then(|| clean_title(&String::from_utf8_lossy(&output.stdout)))
            .flatten()
        else {
            return;
        };
        // A name given meanwhile — `/rename`, or another client — stays.
        let named = client
            .request("thread/read", json!({ "threadId": thread_id }))
            .await
            .is_ok_and(|response| {
                response
                    .pointer("/thread/name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.is_empty())
            });
        if !named {
            let _ = client
                .request(
                    "thread/name/set",
                    json!({ "threadId": thread_id, "name": title }),
                )
                .await;
        }
    });
}

const PROMPT_SUGGESTION_INSTRUCTIONS: &str =
    "Predict the next message the user is most likely to send in the conversation below.
Write it the way the user would type it, in the language the user writes in: one short line, ideally under 80 characters.
If there is no natural next step, reply with nothing. No quotes, labels, or explanations. Do not run commands or use tools.";

/// Claude Code predicts the next prompt after each turn; the Codex app-server
/// has no such event, so a one-off `codex exec` makes the same guess from the
/// last exchange, isolated the way thread titles are. `done` gets the cleaned
/// suggestion, or `None` when the guess failed or came back empty. Dropping the
/// returned task kills the run.
pub(crate) fn spawn_prompt_suggestion(
    codex_path: &Path,
    model: &str,
    conversation: &str,
    done: impl FnOnce(Option<String>) + Send + 'static,
) -> JoinHandle<()> {
    let mut command = codex_invocation(
        &resolve_command(codex_path),
        &[
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "--json",
            "-c",
            "model_reasoning_effort=\"low\"",
            "-m",
            model,
        ],
    );
    command
        .arg("-C")
        .arg(env::temp_dir())
        .arg("-")
        .env_remove("DEVEZCODE_ROOM_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    crate::child_process::isolate_backend(&mut command);
    let input = format!("{PROMPT_SUGGESTION_INSTRUCTIONS}\n\n{conversation}");
    tokio::spawn(async move {
        let Ok(mut child) = command.spawn() else {
            return done(None);
        };
        let _job = crate::child_process::adopt_backend(&child);
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        let output = timeout(Duration::from_secs(60), child.wait_with_output())
            .await
            .ok()
            .and_then(Result::ok)
            .filter(|output| output.status.success());
        done(output.and_then(|output| {
            prompt_suggestion_reply(&String::from_utf8_lossy(&output.stdout))
                .as_deref()
                .and_then(crate::state::clean_prompt_suggestion)
        }));
    })
}

/// The answer in a `codex exec --json` event stream. The guess reads model
/// output that can quote third-party text, so a run that reached for any tool
/// — a command, a search, an MCP call — may have been steered, and its answer
/// is thrown away rather than offered as the user's next prompt.
fn prompt_suggestion_reply(events: &str) -> Option<String> {
    let mut reply = None;
    for event in events.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        let Some(item) = event.get("item") else {
            continue;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("agent_message") => {
                if event.get("type").and_then(Value::as_str) == Some("item.completed") {
                    reply = item.get("text").and_then(Value::as_str).map(ToOwned::to_owned);
                }
            }
            Some("reasoning") => {}
            _ => return None,
        }
    }
    reply
}

/// The reply's first line, without the quotes or markdown a model may add.
pub(crate) fn clean_title(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|line| !line.is_empty())?;
    let title = line
        .trim_start_matches(['#', '*', '"', '\'', '`', '“', '‘', ' '])
        .trim_end_matches(['*', '"', '\'', '`', '”', '’', '.', '。', ' ']);
    // A model counts loosely, so the cap is enforced here: spaces are free, a
    // Korean character weighs 2 and an English letter or digit 1. The cut falls
    // at the last space that fits when it would split a word.
    let mut bytes = 0;
    let overflow = title.char_indices().find(|&(_, c)| {
        if !c.is_whitespace() {
            bytes += if c.is_ascii() { 1 } else { 2 };
        }
        bytes > TITLE_BYTES
    });
    let title = match overflow {
        Some((end, _)) => {
            let fitted = &title[..end];
            fitted.rfind(' ').map_or(fitted, |space| &fitted[..space])
        }
        None => title,
    }
    .trim_end();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The app-server turns `clientInfo.name` into the `originator` header it sends
/// to chatgpt.com, and the connectors endpoints behind Cloudflare only answer
/// the CLI's own originator — anything else gets a bot challenge instead of an
/// answer, which is what made `app/list` fail with a 403 HTML page. The client
/// still identifies itself as devez-vibe everywhere the app-server reports it;
/// only the outgoing header is pinned. An explicit override in the environment
/// wins, so this stays debuggable.
fn apply_originator_override(command: &mut Command) {
    if env::var_os(ORIGINATOR_OVERRIDE_ENV).is_none() {
        command.env(ORIGINATOR_OVERRIDE_ENV, "codex_cli_rs");
    }
}

/// Codex 0.147 keeps the MCP 2026-07-28 protocol — paginated discovery,
/// multi-round requests, and non-blocking server startup — behind an opt-in
/// feature. Turn it on for this invocation so a slow MCP server no longer holds
/// up the turn that needs it. A config that already decided the flag wins, so
/// turning it off stays possible.
fn apply_mcp_2026_protocol_override(command: &mut Command) {
    if codex_config_declares_mcp_2026_protocol() {
        return;
    }
    command.arg("-c").arg(MCP_2026_PROTOCOL_OVERRIDE);
    apply_unstable_features_warning_override(command);
}

/// Codex greets every launch that has an under-development feature on with a
/// warning naming the flag. The flag above is ours, not something the user
/// asked for, so silence the warning we caused — but only when the config has
/// not decided the setting itself.
fn apply_unstable_features_warning_override(command: &mut Command) {
    if codex_config_declares_unstable_features_warning() {
        return;
    }
    command.arg("-c").arg(UNSTABLE_WARNING_OVERRIDE);
}

/// Input typed while Codex answers steers the turn, but without this flag it
/// waits until the whole answer has streamed. With it the answer is cut and the
/// input lands at once (`AppState::settle_cut_answers` keeps the record in
/// order). A config that already decided the flag wins.
fn apply_instant_interrupt_override(command: &mut Command) {
    if codex_config_declares(&[INSTANT_INTERRUPT_KEY, "features.instant_interrupt"]) {
        return;
    }
    command.arg("-c").arg(INSTANT_INTERRUPT_OVERRIDE);
}

/// Codex 0.152 turned the planning tool into an opt-in, so a launch that never
/// asks for it is served no `update_plan` at all and the plan card Devez Vibe
/// draws from those calls would stay empty. Ask for it on this invocation; a
/// config that already decided the setting wins, so turning it off stays
/// possible.
fn apply_update_plan_tool_override(command: &mut Command) {
    if codex_config_declares_update_plan_tool() {
        return;
    }
    command.arg("-c").arg(UPDATE_PLAN_TOOL_OVERRIDE);
}

fn codex_config_declares_update_plan_tool() -> bool {
    crate::state::codex_home()
        .map(|home| home.join("config.toml"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|config| config_declares_update_plan_tool(&config))
}

/// Codex reads custom agents from `~/.codex/agents/`, so the fixed-model lanes
/// the role prompts dispatch have to exist there before the app-server starts.
/// Known shipped prompts are upgraded; customized instructions and settings stay.
/// A write failure is
/// not fatal to the session: the roles fall back to plain subagents with the
/// model named in the dispatch, so it is deliberately not surfaced here.
fn provision_devez_subagents() {
    if let Some(home) = crate::state::codex_home() {
        let _ = crate::subagents::provision_codex_agents(&home);
    }
}

/// `enabled` is far too common a key to match on its own — the plugin sections
/// of an ordinary config are full of it — so the section header has to be
/// folded into the key path first. The dotted form and the table form of the
/// same setting both count as the user's decision.
fn config_declares_update_plan_tool(config: &str) -> bool {
    let mut section: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = header.split('.').map(toml_key).collect();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let mut path = section.clone();
        path.extend(key.split('.').map(toml_key));
        match path.join(".").as_str() {
            "tools.update_plan.enabled" => return true,
            // An inline table decides the same setting one level up.
            "tools.update_plan" if value.contains("enabled") => return true,
            "tools" if value.contains("update_plan") && value.contains("enabled") => return true,
            _ => {}
        }
    }
    false
}

fn toml_key(part: &str) -> String {
    part.trim().trim_matches(['"', '\'']).to_owned()
}

fn codex_config_declares_mcp_2026_protocol() -> bool {
    codex_config_declares(&[MCP_2026_FEATURE_KEY, "features.mcp_2026_07_28"])
}

fn codex_config_declares_unstable_features_warning() -> bool {
    codex_config_declares(&[UNSTABLE_WARNING_KEY])
}

fn codex_config_declares(keys: &[&str]) -> bool {
    crate::state::codex_home()
        .map(|home| home.join("config.toml"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|config| config_declares(&config, keys))
}

/// Both keys are unique to their own setting, so a bare assignment anywhere in
/// the file is the user's decision no matter which section it sits in.
fn config_declares(config: &str, keys: &[&str]) -> bool {
    config.lines().any(|line| {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, _)) = line.split_once('=') else {
            return false;
        };
        keys.contains(&key.trim().trim_matches(['"', '\'']))
    })
}

/// Codex app-server starts stdio MCP children with an isolated environment.
/// Put the DevezCode room and bridge discovery variable names in this
/// invocation's MCP overrides so every browser call is bound to the tab that
/// started Devez Vibe, without mutating global config or persisting the token.
///
/// The overrides only carry nested `env` / `env_vars`, so on a Codex home that
/// never declared the browser server they would create an entry with neither
/// `command` nor `url` and Codex would refuse the whole config with
/// `invalid transport`, taking the app-server down with it. Skip the overrides
/// there: the browser MCP does not exist in that install anyway.
fn apply_devezcode_room_override(command: &mut Command, room: Option<&str>) {
    if !codex_config_declares_devez_browser() {
        return;
    }
    for override_value in devezcode_browser_overrides(room) {
        command.arg("-c").arg(override_value);
    }
}

fn codex_config_declares_devez_browser() -> bool {
    crate::state::codex_home()
        .map(|home| home.join("config.toml"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|config| config_declares_devez_browser(&config))
}

fn config_declares_devez_browser(config: &str) -> bool {
    config.lines().any(|line| {
        let line = line.split('#').next().unwrap_or_default().trim();
        matches!(
            line,
            "[mcp_servers.devez-browser]"
                | "[mcp_servers.\"devez-browser\"]"
                | "[mcp_servers.'devez-browser']"
        )
    })
}

fn devezcode_room_override(room: Option<&str>) -> Option<String> {
    room.filter(|room| !room.is_empty()).map(|room| {
        format!(
            "mcp_servers.devez-browser.env.DEVEZCODE_ROOM_ID={}",
            toml_string(room)
        )
    })
}

fn devezcode_browser_overrides(room: Option<&str>) -> Vec<String> {
    let Some(room_override) = devezcode_room_override(room) else {
        return Vec::new();
    };
    vec![
        room_override,
        "mcp_servers.devez-browser.env_vars=[\"DEVEZCODE_BRIDGE_PIPE\",\"DEVEZCODE_BRIDGE_TOKEN\"]"
            .to_string(),
    ]
}

fn toml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

const ORIGINATOR_OVERRIDE_ENV: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
const MCP_2026_FEATURE_KEY: &str = "mcp_2026_07_28";
const MCP_2026_PROTOCOL_OVERRIDE: &str = "features.mcp_2026_07_28=true";
const INSTANT_INTERRUPT_KEY: &str = "instant_interrupt";
const INSTANT_INTERRUPT_OVERRIDE: &str = "features.instant_interrupt=true";
const UPDATE_PLAN_TOOL_OVERRIDE: &str = "tools.update_plan.enabled=true";
const UNSTABLE_WARNING_KEY: &str = "suppress_unstable_features_warning";
const UNSTABLE_WARNING_OVERRIDE: &str = "suppress_unstable_features_warning=true";

pub(crate) fn resolve_command(command: &Path) -> PathBuf {
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

async fn request_with_permissions(
    client: &AppServerClient,
    method: &str,
    params: Value,
) -> Result<(Value, String, bool)> {
    if !params.is_object() {
        bail!(tr(
            "Codex 권한 요청의 매개변수는 객체여야 합니다.",
            "The Codex permission request parameters must be an object.",
        ));
    }
    let requested = params.get("permissions").and_then(Value::as_str).unwrap_or(":danger-full-access");
    let descending = [":danger-full-access", ":workspace", ":read-only"];
    // Explicit role restrictions, including read-only review turns, still apply.
    let profiles = if requested == descending[0] { &descending[..] } else { std::slice::from_ref(&requested) };
    for (index, profile) in profiles.iter().enumerate() {
        let mut attempt = params.clone();
        attempt["permissions"] = json!(profile);
        if *profile == ":danger-full-access" {
            attempt["approvalPolicy"] = json!("never");
        } else if index > 0 {
            // A restricted mode inherits the runtime's permitted approval policy.
            attempt.as_object_mut().unwrap().remove("approvalPolicy");
            attempt.as_object_mut().unwrap().remove("approvalsReviewer");
        }
        let mut response = client.request(method, attempt.clone()).await;
        if method == "turn/start"
            && let Some(thread) = attempt.get("threadId").and_then(Value::as_str)
            && response.as_ref().is_err_and(|error| error.to_string() == format!("turn/start: thread not found: {thread} (-32600)"))
        {
            // A restarted app-server has none of the router's saved threads
            // loaded. Only this pre-turn failure is safe to retry.
            let mut resume = json!({ "threadId": thread, "excludeTurns": true });
            for key in ["model", "cwd", "serviceTier", "permissions", "approvalPolicy", "approvalsReviewer"] {
                if let Some(value) = attempt.get(key) {
                    resume[key] = value.clone();
                }
            }
            response = match client.request("thread/resume", resume).await {
                Ok(_) => client.request(method, attempt).await,
                Err(error) => Err(error),
            };
        }
        match response {
            Ok(response) => {
                let actual = response.pointer("/activePermissionProfile/id")
                    .and_then(Value::as_str).unwrap_or(profile).to_owned();
                return Ok((response, actual, index > 0));
            }
            Err(error) if index + 1 < profiles.len() && permission_configuration_rejected(&error.to_string()) => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!("a permissions candidate is always present")
}

fn permission_configuration_rejected(error: &str) -> bool {
    let message = error.to_ascii_lowercase().replace(['_', '-'], " ");
    let setting = (message.contains("permission") && message.contains("profile"))
        || message.contains("sandbox mode") || message.contains("sandbox policy")
        || message.contains("approval policy") || message.contains("approvals reviewer");
    setting && ["not allowed", "not permitted", "disallowed", "forbidden", "requirement", "disabled", "restricted"]
        .iter().any(|rejection| message.contains(rejection))
}

async fn route_message(
    message: Value,
    pending: &PendingMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && (message.get("result").is_some() || message.get("error").is_some())
    {
        if let Some(sender) = pending.lock().expect("pending mutex").remove(&id) {
            let response = if let Some(error) = message.get("error") {
                Err(format_rpc_error(error))
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(response);
        }
        return;
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let _ = events.send(ServerEvent::ProtocolWarning(
            tr("method 없는 app-server 메시지를 무시했습니다.", "Ignored an app-server message without a method.")
                .to_owned(),
        ));
        return;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    if let Some(id) = message.get("id") {
        let _ = events.send(ServerEvent::Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        });
    } else {
        let _ = events.send(ServerEvent::Notification {
            method: method.to_owned(),
            params,
        });
    }
}

fn format_rpc_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = condense_error_message(
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(tr("알 수 없는 app-server 오류", "Unknown app-server error")),
    );
    match code {
        Some(code) => format!("{message} ({code})"),
        None => message,
    }
}

/// An upstream failure often arrives with a whole HTML error page or a JSON
/// response body attached. Notices only have room for the part a person can
/// act on, so keep what comes before the markup, read a JSON body as its inner
/// message, and cap each of the first two lines — a guide line can precede the
/// error it explains.
pub fn condense_error_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let markup = ["<html", "<!doctype", "<head", "<body", "<?xml"]
        .iter()
        .filter_map(|tag| lower.find(tag))
        .min()
        .unwrap_or(message.len());
    let head = message[..markup].trim();
    // A message that is nothing but markup still has to say something.
    let head = if head.is_empty() { message } else { head };
    let lines = head
        .lines()
        .map(condense_error_line)
        .filter(|line| !line.is_empty())
        .take(2)
        .collect::<Vec<_>>();
    lines.join("\n")
}

fn condense_error_line(line: &str) -> String {
    const LIMIT: usize = 200;
    let line = line.trim();
    // `API Error: 401 {"type":"error","error":{"message":"OAuth token has
    // expired."}}` reads as `API Error: 401 OAuth token has expired.`
    let inner = line.find('{').and_then(|start| {
        let mut stream = serde_json::Deserializer::from_str(&line[start..]).into_iter::<Value>();
        let body = stream.next()?.ok()?;
        let rest = &line[start + stream.byte_offset()..];
        let message = body
            .pointer("/error/message")
            .or_else(|| body.get("message"))?
            .as_str()?;
        Some(format!("{} {message}{rest}", line[..start].trim_end()).trim().to_owned())
    });
    let line = inner.as_deref().unwrap_or(line);
    let line = line.trim_end_matches([':', '-', '·']).trim_end();
    if line.chars().count() > LIMIT {
        format!(
            "{}…",
            line.chars().take(LIMIT).collect::<String>().trim_end()
        )
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_suggestion_run_that_used_a_tool_is_discarded() {
        let answer = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"run the tests"}}"#;
        let reasoning = r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"…"}}"#;
        let command = r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"cat ~/.codex/auth.json"}}"#;
        let search = r#"{"type":"item.started","item":{"id":"item_3","type":"web_search","query":"x"}}"#;
        let turn = r#"{"type":"turn.completed","usage":{}}"#;

        assert_eq!(
            prompt_suggestion_reply(&[reasoning, answer, turn].join("\n")).as_deref(),
            Some("run the tests")
        );
        assert_eq!(prompt_suggestion_reply(&[command, answer].join("\n")), None);
        assert_eq!(prompt_suggestion_reply(&[answer, search].join("\n")), None);
        assert_eq!(prompt_suggestion_reply("run the tests"), None, "plain text is not an event stream");
    }

    struct CodexLauncherFixture(PathBuf);

    impl CodexLauncherFixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "dvz codex launcher {} {}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for CodexLauncherFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn codex_invocation_preserves_explicit_launcher_and_arguments() {
        for selected in [
            "codex",
            "C:/Program Files/Codex/codex.exe",
            "C:/custom/codex.CMD",
            "C:/custom/launch.bat",
        ] {
            let args = ["exec", "-c", "model_reasoning_effort=\"low\""];
            let mut command = codex_invocation(Path::new(selected), &args);
            command.arg("path with spaces");
            let command = command.as_std();
            assert_eq!(command.get_program(), std::ffi::OsStr::new(selected));
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [args.as_slice(), &["path with spaces"]].concat()
            );
            assert!(command.get_envs().next().is_none());
            assert!(command.get_current_dir().is_none());
        }
    }

    #[test]
    fn codex_invocation_does_not_substitute_stale_vendored_binaries() {
        let fixture = CodexLauncherFixture::new();
        let shim = fixture.write("codex.cmd", "@echo off\r\n");
        fixture.write(
            "node_modules/@openai/codex/bin/codex.js",
            "// official wrapper owns package resolution",
        );
        fixture.write(
            "node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/package.json",
            "{}",
        );
        for triple in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
            fixture.write(
                &format!("node_modules/@openai/codex/vendor/{triple}/bin/codex.exe"),
                "stale placeholder; never executed",
            );
        }
        let command = codex_command(&shim);
        assert_eq!(command.as_std().get_program(), shim.as_os_str());
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["app-server", "--listen", "stdio://"]
        );
    }

    #[test]
    fn codex_invocation_leaves_orphaned_nested_binary_selection_to_the_wrapper() {
        let fixture = CodexLauncherFixture::new();
        let shim = fixture.write("codex.cmd", "@echo off\r\n");
        fixture.write(
            "node_modules/@openai/codex/bin/codex.js",
            "// official wrapper uses require.resolve",
        );
        for (package, triple) in [
            ("codex-win32-x64", "x86_64-pc-windows-msvc"),
            ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        ] {
            fixture.write(
                &format!(
                    "node_modules/@openai/codex/node_modules/@openai/{package}/vendor/{triple}/bin/codex.exe"
                ),
                "orphaned placeholder; no package.json",
            );
            fixture.write(&format!("node_modules/@openai/{package}/package.json"), "{}");
            fixture.write(
                &format!("node_modules/@openai/{package}/vendor/{triple}/bin/codex.exe"),
                "selected by the official wrapper; never executed",
            );
        }
        assert_eq!(codex_command(&shim).as_std().get_program(), shim.as_os_str());
    }

    #[test]
    fn codex_invocation_does_not_replace_custom_shims_with_adjacent_npm_scripts() {
        let fixture = CodexLauncherFixture::new();
        fixture.write(
            "node_modules/@openai/codex/bin/codex.js",
            "// not the selected custom launcher",
        );
        for selected in ["codex.cmd", "custom-launch.bat"] {
            let shim = fixture.write(selected, "@echo off\r\nset CUSTOM_LAUNCHER=1\r\n");
            let command = codex_command(&shim);
            assert_eq!(command.as_std().get_program(), shim.as_os_str());
        }
    }

    // Only the batch fixture child sets this marker. A normal test run does not
    // block on stdin, and the fixture needs no Node.js or Codex installation.
    #[cfg(windows)]
    #[test]
    fn codex_invocation_batch_probe_child() {
        if env::var("DVZ_LAUNCHER_TEST_MARKER").as_deref() != Ok("from selected shim") {
            return;
        }
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).unwrap();
        println!(
            "DVZ_LAUNCHER_TEST_RESULT={}",
            json!({
                "args": env::args().skip(1).collect::<Vec<_>>(),
                "marker": env::var("DVZ_LAUNCHER_TEST_MARKER").unwrap(),
                "input": input,
            })
        );
    }

    // Exercise the real Windows batch parser and an npm-style %* relay, not a
    // hand-written model of cmd quoting. The test executable is the argv probe;
    // --skip carries our literal test arguments without treating them as flags.
    #[cfg(windows)]
    #[tokio::test]
    async fn codex_invocation_batch_round_trips_quotes_environment_and_stdio() {
        let fixture = CodexLauncherFixture::new();
        fixture.write(
            "node_modules/@openai/codex/bin/codex.js",
            "throw new Error('custom launcher was bypassed');",
        );
        let args = [
            "--exact",
            "app_server::tests::codex_invocation_batch_probe_child",
            "--nocapture",
            "--skip",
            "-c",
            "--skip",
            "model_reasoning_effort=\"low\"",
            "--skip",
            "path with spaces",
            "--skip",
            "%DVZ_LAUNCHER_TEST_MARKER%",
            "--skip",
            "!DVZ_LAUNCHER_TEST_MARKER!",
            "--skip",
            "a&b|c<d>e^f",
            "--skip",
            "C:\\trailing path\\",
        ];
        for extension in ["cmd", "bat", "CMD"] {
            let shim = fixture.write(
                &format!("custom launcher.{extension}"),
                concat!(
                    "@echo off\r\n",
                    "setlocal\r\n",
                    "set \"DVZ_LAUNCHER_TEST_MARKER=from selected shim\"\r\n",
                    "\"%DVZ_LAUNCHER_TEST_EXE%\" %*\r\n",
                    "exit /b %errorlevel%\r\n",
                ),
            );
            let mut command = codex_invocation(&shim, &args[..3]);
            command
                .args(&args[3..])
                .env("DVZ_LAUNCHER_TEST_EXE", env::current_exe().unwrap())
                .env("DVZ_LAUNCHER_TEST_MARKER", "before shim")
                .current_dir(&fixture.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            crate::child_process::isolate_backend(&mut command);
            let mut child = command.spawn().unwrap();
            let _job = crate::child_process::adopt_backend(&child);
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"{\"jsonrpc\":\"2.0\"}\n")
                .await
                .unwrap();
            let output = timeout(Duration::from_secs(10), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let result = stdout
                .lines()
                .find_map(|line| {
                    line.split_once("DVZ_LAUNCHER_TEST_RESULT=")
                        .map(|(_, value)| value)
                })
                .expect("batch wrapper did not run the Rust argv probe");
            let result: Value = serde_json::from_str(result).unwrap();
            assert_eq!(result["args"], json!(args));
            assert_eq!(result["marker"], "from selected shim");
            assert_eq!(result["input"], "{\"jsonrpc\":\"2.0\"}\n");
        }
    }

    fn computer_test_client() -> (AppServerClient, mpsc::UnboundedReceiver<Value>) {
        let (outbound, messages) = mpsc::unbounded_channel();
        (AppServerClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
        }, messages)
    }

    fn observe_computer_turn(observer: &crate::computer_use::Observer, thread: &str, turn: &str, server: &str) {
        observer.observe(&json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":turn}}}));
        observer.observe(&json!({"method":"item/started","params":{"threadId":thread,"turnId":turn,"item":{
            "id":"computer-call","type":"mcpToolCall","server":server,"tool":"js","arguments":{"code":"await sky.click({x:1,y:2})"}
        }}}));
    }

    async fn acknowledge_computer_interrupt(client: &AppServerClient, request: &Value, events: &mpsc::UnboundedSender<ServerEvent>, error: bool) {
        let response = if error { json!({"id":request["id"],"error":{"code":-32000,"message":"temporary stop failure"}}) }
            else { json!({"id":request["id"],"result":{}}) };
        route_message(response, &client.pending, events).await;
    }

    #[tokio::test]
    async fn physical_escape_after_computer_completion_does_not_send_interrupt() {
        let (client, mut messages) = computer_test_client();
        let (events, _) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let monitor = crate::computer_use::Monitor::start_with_test_input(client, events, None, input_rx);
        let observer = monitor.observer();
        observe_computer_turn(&observer, "a", "1", "node_repl");
        let generation = monitor.generation();
        observer.observe(&json!({"method":"item/completed","params":{
            "threadId":"a","turnId":"1","item":{"id":"computer-call","type":"mcpToolCall","server":"node_repl","tool":"js"}
        }}));
        input.send(generation).unwrap();
        assert!(timeout(Duration::from_millis(200), messages.recv()).await.is_err());
        monitor.stop();
    }

    #[tokio::test]
    async fn physical_escape_interrupts_the_native_pipe_turn_without_a_marker() {
        let (client, mut messages) = computer_test_client();
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let monitor = crate::computer_use::Monitor::start_with_test_input(client.clone(), events.clone(), None, input_rx);
        let observer = monitor.observer();
        input.send(0).unwrap(); // Escape before any CU turn cannot cancel later work.
        tokio::task::yield_now().await;
        observe_computer_turn(&observer, "native-pipe-thread", "turn-1", "cua_repl");
        input.send(monitor.generation()).unwrap();
        let request = timeout(Duration::from_secs(1), messages.recv()).await.unwrap().unwrap();
        assert_eq!(request["method"], "turn/interrupt");
        assert_eq!(request["params"], json!({"threadId":"native-pipe-thread","turnId":"turn-1"}));
        acknowledge_computer_interrupt(&client, &request, &events, false).await;
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        assert!(event_rx.try_recv().is_err());
        observer.observe(&json!({"method":"turn/completed","params":{"threadId":"native-pipe-thread","turn":{"id":"turn-1","status":"interrupted"}}}));
        input.send(monitor.generation()).unwrap();
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        monitor.stop();
    }

    #[tokio::test]
    async fn queued_escape_keeps_its_original_scope_when_another_session_starts() {
        let (client, mut messages) = computer_test_client();
        let (events, _) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let monitor = crate::computer_use::Monitor::start_with_test_input(client.clone(), events.clone(), None, input_rx);
        let observer = monitor.observer();
        observe_computer_turn(&observer, "a", "1", "cua_repl");
        let generation = monitor.generation();
        observe_computer_turn(&observer, "b", "1", "cua_repl");
        input.send(generation).unwrap();
        let request = timeout(Duration::from_secs(1), messages.recv()).await.unwrap().unwrap();
        assert_eq!(request["params"], json!({"threadId":"a","turnId":"1"}));
        acknowledge_computer_interrupt(&client, &request, &events, false).await;
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        observer.observe(&json!({"method":"turn/completed","params":{"threadId":"a","turn":{"id":"1"}}}));
        observe_computer_turn(&observer, "a", "2", "cua_repl");
        input.send(generation).unwrap();
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        monitor.stop();
    }

    #[tokio::test]
    async fn physical_escape_failure_reports_and_allows_a_fresh_press_retry() {
        let (client, mut messages) = computer_test_client();
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let monitor = crate::computer_use::Monitor::start_with_test_input(client.clone(), events.clone(), None, input_rx);
        observe_computer_turn(&monitor.observer(), "a", "1", "node_repl");
        input.send(monitor.generation()).unwrap();
        let request = timeout(Duration::from_secs(1), messages.recv()).await.unwrap().unwrap();
        monitor.observer().observe(&json!({"method":"item/completed","params":{
            "threadId":"a","turnId":"1","item":{"id":"computer-call","type":"mcpToolCall","server":"node_repl","tool":"js"}
        }}));
        input.send(monitor.generation()).unwrap(); // a pending RPC is never duplicated
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        acknowledge_computer_interrupt(&client, &request, &events, true).await;
        assert!(matches!(timeout(Duration::from_secs(1), event_rx.recv()).await.unwrap(), Some(ServerEvent::ProtocolWarning(_))));
        // The second press happened while A was pending; failure must preserve it.
        let retry = timeout(Duration::from_secs(1), messages.recv()).await.unwrap().unwrap();
        assert_eq!(retry["params"], request["params"]);
        acknowledge_computer_interrupt(&client, &retry, &events, false).await;
        monitor.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_deadline_removes_pending_response_and_allows_retry() {
        let (client, mut messages) = computer_test_client();
        let waiting = tokio::spawn({ let client = client.clone(); async move {
            client.request("turn/interrupt", json!({"threadId":"a","turnId":"1"})).await
        }});
        let request = messages.recv().await.unwrap();
        assert!(client.pending.lock().expect("pending mutex").contains_key(&request["id"].as_u64().unwrap()));
        tokio::time::advance(INTERRUPT_TIMEOUT).await;
        assert!(waiting.await.unwrap().is_err());
        assert!(client.pending.lock().expect("pending mutex").is_empty());
        let (events, _) = mpsc::unbounded_channel();
        acknowledge_computer_interrupt(&client, &request, &events, false).await; // late ack harmless
        assert!(client.pending.lock().expect("pending mutex").is_empty());
    }

    #[tokio::test]
    async fn closing_connection_wakes_a_retained_client_without_reader_cleanup() {
        let (client, mut messages) = computer_test_client();
        let request = tokio::spawn({ let client = client.clone(); async move {
            client.request("thread/read", json!({"threadId":"a"})).await
        }});
        messages.recv().await.unwrap();
        client.close(); // the same synchronous path used by AppServer::drop
        assert!(timeout(Duration::from_secs(1), request).await.unwrap().unwrap().is_err());
        assert!(client.pending.lock().expect("pending mutex").is_empty());
        assert!(client.request("thread/read", json!({"threadId":"a"})).await.is_err());
    }

    #[tokio::test]
    async fn cancelling_a_request_removes_its_pending_response() {
        let (client, mut messages) = computer_test_client();
        let waiting = tokio::spawn({ let client = client.clone(); async move {
            client.request("thread/read", json!({"threadId":"a"})).await
        }});
        let request = messages.recv().await.unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(client.pending.lock().expect("pending mutex").is_empty());
        let (events, _) = mpsc::unbounded_channel();
        route_message(json!({"id":request["id"],"result":{}}), &client.pending, &events).await;
        assert!(client.pending.lock().expect("pending mutex").is_empty());
    }

    #[tokio::test]
    async fn broken_stdin_wakes_requests_even_if_stdout_stays_open() {
        let (client, messages) = computer_test_client();
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let (stdin, server_stdin) = tokio::io::duplex(1024);
        drop(server_stdin);
        let writer = tokio::spawn(write_messages(stdin, messages, client.clone(), events));
        let result = timeout(Duration::from_secs(1), client.request("thread/read", json!({"threadId":"a"})))
            .await.expect("a broken stdin must not wait for the RPC deadline");
        assert!(result.is_err());
        writer.await.unwrap();
        assert!(matches!(event_rx.recv().await, Some(ServerEvent::Closed(_))));
        assert!(client.pending.lock().expect("pending mutex").is_empty());
        assert!(client.notify("initialized", None).is_err());
    }

    #[tokio::test]
    async fn stopping_computer_monitor_prevents_late_events_from_rearming_it() {
        let (client, mut messages) = computer_test_client();
        let (events, _) = mpsc::unbounded_channel();
        let (input, input_rx) = mpsc::unbounded_channel();
        let monitor = crate::computer_use::Monitor::start_with_test_input(client, events, None, input_rx);
        let observer = monitor.observer();
        observe_computer_turn(&observer, "a", "1", "cua_repl");
        let generation = monitor.generation();
        monitor.stop();
        observe_computer_turn(&observer, "b", "1", "cua_repl");
        observer.recover();
        assert_eq!(monitor.generation(), generation);
        let _ = input.send(generation);
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn computer_use_reloads_changed_pipes_and_retries_failed_reload() {
        let path = std::env::temp_dir().join(format!("dvz-computer-use-{}-{}.toml",
            std::process::id(), std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let original = "[mcp_servers.node_repl.env]\nSKY_CUA_NATIVE_PIPE_DIRECTORY = 'old'\n";
        std::fs::write(&path, original).unwrap();
        let (outbound, mut messages) = mpsc::unbounded_channel();
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let client = AppServerClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            pending: pending.clone(), next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
        };
        let cached = Arc::new(Mutex::new(computer_use_pipe_config(original)));
        refresh_computer_use(&client, &path, &cached).await.unwrap();
        std::fs::write(&path, format!("{original}model = 'unrelated'\n")).unwrap();
        refresh_computer_use(&client, &path, &cached).await.unwrap();
        assert!(messages.try_recv().is_err(), "unchanged pipes must not restart MCP servers");

        let (events, _) = mpsc::unbounded_channel();
        for (config, fail) in [
            ("mcp_servers.node_repl.env.SKY_CUA_NATIVE_PIPE_DIRECTORY = 'new'\n", true),
            ("mcp_servers.node_repl.env.SKY_CUA_NATIVE_PIPE_DIRECTORY = 'new'\n", false),
            ("# SKY_CUA_NATIVE_PIPE_DIRECTORY = 'ignored'\n", false),
        ] {
            std::fs::write(&path, config).unwrap();
            let previous = cached.lock().await.clone();
            let task = tokio::spawn({
                let (client, path, cached) = (client.clone(), path.clone(), cached.clone());
                async move { refresh_computer_use(&client, &path, &cached).await }
            });
            let request = timeout(Duration::from_secs(2), messages.recv()).await.unwrap().unwrap();
            assert_eq!(request["method"], "config/mcpServer/reload");
            assert_eq!(request["params"], json!({}));
            let reply = if fail {
                json!({"id": request["id"], "error": {"message": "reload failed"}})
            } else {
                json!({"id": request["id"], "result": {}})
            };
            route_message(reply, &pending, &events).await;
            let result = task.await.unwrap();
            assert_eq!(result.is_err(), fail);
            if let Err(error) = result {
                assert!(error.to_string().contains("reload failed"));
            }
            assert_eq!(*cached.lock().await, if fail { previous } else { computer_use_pipe_config(config) });
        }
        std::fs::remove_file(&path).unwrap();
        refresh_computer_use(&client, &path, &cached).await.unwrap();
        assert!(messages.try_recv().is_err(), "unreadable settings must not interrupt development");
    }

    /// 백엔드가 살아 있는 채로 응답만 멈추면 요청을 끊고 알린다. 시간을 멈춘
    /// 검사라 실제로 10분을 기다리지 않는다.
    #[tokio::test(start_paused = true)]
    async fn a_silent_backend_ends_the_request_instead_of_hanging() {
        let (outbound, _inbox) = mpsc::unbounded_channel();
        let client = AppServerClient {
            outbound: Arc::new(StdMutex::new(Some(outbound))),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
        };

        let error = client
            .request("session/start", json!({}))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("응답하지 않아"), "{error}");
        assert!(
            client.pending.lock().expect("pending mutex").is_empty(),
            "끊은 요청은 대기 목록에 남기지 않는다"
        );
    }

    #[tokio::test]
    async fn permission_fallback_only_retries_rejected_configuration() {
        let denied = "permission profile :danger-full-access is not allowed by requirements";
        for (requested, replies, profiles) in [
            (":danger-full-access", vec![None], vec![":danger-full-access"]),
            (":danger-full-access", vec![Some(denied), None], vec![":danger-full-access", ":workspace"]),
            (":danger-full-access", vec![Some(denied), Some("sandbox mode workspace-write is not allowed"), None], vec![":danger-full-access", ":workspace", ":read-only"]),
            (":danger-full-access", vec![Some(denied), Some(denied), Some(denied)], vec![":danger-full-access", ":workspace", ":read-only"]),
            (":danger-full-access", vec![Some("approval_policy never is not allowed"), None], vec![":danger-full-access", ":workspace"]),
            (":danger-full-access", vec![Some("authentication expired")], vec![":danger-full-access"]),
            (":danger-full-access", vec![Some("command execution failed: permission denied")], vec![":danger-full-access"]),
            (":danger-full-access", vec![Some("thread not found: another-thread")], vec![":danger-full-access"]),
            (":danger-full-access", vec![Some("thread not found: thread-other")], vec![":danger-full-access"]),
            (":danger-full-access", vec![Some("quoted error: thread not found: thread")], vec![":danger-full-access"]),
            (":read-only", vec![Some(denied)], vec![":read-only"]),
        ] {
            let (outbound, mut messages) = mpsc::unbounded_channel();
            let pending = Arc::new(StdMutex::new(HashMap::new()));
            let client = AppServerClient {
                outbound: Arc::new(StdMutex::new(Some(outbound))),
                pending: pending.clone(), next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
            };
            let params = json!({"threadId": "thread", "permissions": requested, "approvalPolicy": "on-request",
                "model": "model", "input": [{"type": "text", "text": "사용자 작업"}]});
            let task = tokio::spawn(async move { request_with_permissions(&client, "turn/start", params).await });
            let (events, _) = mpsc::unbounded_channel();
            for (index, (reply, profile)) in replies.iter().zip(&profiles).enumerate() {
                let message = timeout(Duration::from_secs(2), messages.recv()).await.unwrap().unwrap();
                assert_eq!(message["params"]["permissions"], *profile);
                assert_eq!(message["params"]["threadId"], "thread");
                assert_eq!(message["params"]["input"][0]["text"], "사용자 작업");
                if *profile == ":danger-full-access" {
                    assert_eq!(message["params"]["approvalPolicy"], "never");
                } else if index > 0 {
                    assert!(message["params"].get("approvalPolicy").is_none());
                }
                let response = if let Some(error) = reply {
                    json!({"id": message["id"], "error": {"code": -32600, "message": error}})
                } else {
                    json!({"id": message["id"], "result": {"turn": {"id": "new-turn"}}})
                };
                route_message(response, &pending, &events).await;
            }
            let result = task.await.unwrap();
            assert_eq!(result.is_ok(), replies.last() == Some(&None));
            if let Ok((_, profile, lowered)) = result {
                assert_eq!(profile, *profiles.last().unwrap());
                assert_eq!(lowered, profiles.len() > 1);
            }
            assert!(messages.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn an_unloaded_codex_turn_resumes_once_before_retrying() {
        let (client, mut messages) = computer_test_client();
        let (events, _) = mpsc::unbounded_channel();
        let params = json!({"threadId":"saved-thread","model":"gpt-6-luna","effort":"low",
            "permissions":":read-only","input":[{"type":"text","text":"이어 하기"}]});
        let task = tokio::spawn({ let client = client.clone(); let params = params.clone(); async move {
            request_with_permissions(&client, "turn/start", params).await
        }});
        let first = messages.recv().await.unwrap();
        route_message(json!({"id":first["id"],"error":{"code":-32600,"message":"thread not found: saved-thread"}}), &client.pending, &events).await;
        let resume = timeout(Duration::from_secs(1), messages.recv()).await.unwrap().unwrap();
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "saved-thread");
        assert_eq!(resume["params"]["permissions"], ":read-only");
        assert!(resume["params"].get("input").is_none());
        route_message(json!({"id":resume["id"],"result":{"thread":{"id":"saved-thread"}}}), &client.pending, &events).await;
        let retry = messages.recv().await.unwrap();
        assert_eq!(retry["method"], "turn/start");
        assert_eq!(retry["params"], params);
        route_message(json!({"id":retry["id"],"result":{"turn":{"id":"next"}}}), &client.pending, &events).await;
        assert!(task.await.unwrap().is_ok());
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn similar_thread_errors_do_not_resume_or_repeat_a_turn() {
        for (code, message) in [
            (-32600, "thread not found: saved-thread-other"),
            (-32603, "thread not found: saved-thread"),
            (-32000, "thread not found: saved-thread"),
            (-32600, "quoted error: thread not found: saved-thread"),
            (-32600, "\"thread not found: saved-thread\""),
        ] {
            let (client, mut messages) = computer_test_client();
            let (events, _) = mpsc::unbounded_channel();
            let task = tokio::spawn({ let client = client.clone(); async move {
                request_with_permissions(&client, "turn/start", json!({"threadId":"saved-thread"})).await
            }});
            let request = messages.recv().await.unwrap();
            route_message(json!({"id":request["id"],"error":{"code":code,"message":message}}), &client.pending, &events).await;
            let result = timeout(Duration::from_secs(1), task).await
                .expect("a different failure must not resume the thread").unwrap();
            assert!(result.is_err(), "{code}: {message}");
            assert!(messages.try_recv().is_err(), "{code}: {message}");
        }
    }

    #[tokio::test]
    #[ignore = "설치된 Codex의 권한 프로필과 응답 확인"]
    async fn live_codex_permission_profiles() {
        let mut server = AppServer::spawn(Path::new("codex"), None).await.unwrap();
        server.initialize().await.unwrap();
        let profiles = server.client.request("permissionProfile/list", json!({})).await.unwrap();
        println!("지원 권한: {profiles}");
        for profile in [":danger-full-access", ":workspace", ":read-only"] {
            let response = server.client.request("thread/start", json!({
                "model": "gpt-5.6-sol", "ephemeral": true, "cwd": std::env::temp_dir(),
                "permissions": profile, "approvalPolicy": "never"
            })).await.unwrap();
            println!("적용 권한: {}", json!({"profile": response["activePermissionProfile"], "approvalPolicy": response["approvalPolicy"], "reviewer": response["approvalsReviewer"], "sandbox": response["sandbox"]["type"]}));
        }
        let response = server.request("thread/start", json!({"cwd": std::env::temp_dir(), "model": "gpt-5.6-sol",
            "ephemeral": false, "approvalPolicy": "on-request"})).await.unwrap();
        assert_eq!(response["approvalPolicy"], "never");
        assert_eq!(response["activePermissionProfile"]["id"], ":danger-full-access");
        // A thread is resumable only after its first turn has created a rollout.
        server.request("turn/start", json!({"threadId": response["thread"]["id"], "model": "gpt-5.6-sol", "effort": "low",
            "input": [{"type": "text", "text": "도구를 쓰지 말고 확인이라는 한 단어로만 답하세요."}]})).await.unwrap();
        loop {
            match timeout(Duration::from_secs(60), server.next_event()).await.unwrap().unwrap() {
                ServerEvent::Notification { method, params } if method == "turn/completed" => {
                    assert_eq!(params["turn"]["status"], "completed");
                    break;
                }
                ServerEvent::Request { method, .. } => panic!("예상하지 않은 요청: {method}"),
                ServerEvent::Closed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let resumed = server.request("thread/resume", json!({"threadId": response["thread"]["id"],
            "approvalPolicy": "on-request"})).await.unwrap();
        assert_eq!(resumed["approvalPolicy"], "never");
        assert_eq!(resumed["activePermissionProfile"]["id"], ":danger-full-access");
        println!("새 대화와 재개에서 전체 접근·승인 요청 없음 확인");
        server.shutdown().await;
    }


    #[test]
    fn initialize_enables_interactive_mcp_forms() {
        let params = initialize_params();

        assert_eq!(
            params
                .pointer("/capabilities/mcpServerOpenaiFormElicitation")
                .and_then(Value::as_bool),
            Some(true)
        );
        // Codex 0.147 reads the extension declaration instead of the alias.
        assert_eq!(
            params.pointer("/capabilities/extensions/openai~1form"),
            Some(&json!({}))
        );
        assert_eq!(
            params.pointer("/clientInfo/name").and_then(Value::as_str),
            Some("devez-vibe")
        );
    }

    #[test]
    fn the_mcp_2026_protocol_stays_opt_in_through_the_config() {
        let keys = [MCP_2026_FEATURE_KEY, "features.mcp_2026_07_28"];
        // A config that says nothing leaves the launch override in charge.
        assert!(!config_declares(
            "model = \"gpt-5\"\n[features]\ntool_search = true\n",
            &keys
        ));
        assert!(config_declares(
            "[features]\nmcp_2026_07_28 = false\n",
            &keys
        ));
        assert!(config_declares(
            "features.mcp_2026_07_28 = true  # already decided\n",
            &keys
        ));
        // A commented-out line is not a decision.
        assert!(!config_declares(
            "[features]\n# mcp_2026_07_28 = true\n",
            &keys
        ));

        let mut command = codex_command(Path::new("codex"));
        apply_mcp_2026_protocol_override(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // The real Codex home decides, so only assert the pair stays together.
        if let Some(index) = args
            .iter()
            .position(|arg| arg == MCP_2026_PROTOCOL_OVERRIDE)
        {
            assert_eq!(args.get(index - 1).map(String::as_str), Some("-c"));
        } else {
            assert!(codex_config_declares_mcp_2026_protocol());
        }
    }

    #[test]
    fn thread_titles_keep_only_the_first_plain_line() {
        assert_eq!(
            clean_title("\n\"세션 자동 이름\".\nextra").as_deref(),
            Some("세션 자동 이름")
        );
        assert_eq!(
            clean_title("## **Fix login flow**").as_deref(),
            Some("Fix login flow")
        );
        assert_eq!(clean_title(" \n \n"), None);
        assert_eq!(
            clean_title(&"가".repeat(80)).map(|title| title.chars().count()),
            Some(TITLE_BYTES / 2)
        );
        // Spaces are free, Korean weighs 2 and ASCII 1; past 24 the cut falls at a space.
        assert_eq!(
            clean_title("세션 자동 이름 생성 규칙 변경").as_deref(),
            Some("세션 자동 이름 생성 규칙 변경")
        );
        assert_eq!(
            clean_title("세션 자동 이름 생성 규칙을 변경").as_deref(),
            Some("세션 자동 이름 생성 규칙을")
        );
        assert_eq!(
            clean_title("rename 기능 12개 추가").as_deref(),
            Some("rename 기능 12개 추가")
        );
        assert_eq!(
            clean_title("Fix the login flow for OAuth users on Windows").as_deref(),
            Some("Fix the login flow for OAuth")
        );
    }

    #[test]
    fn instant_interrupt_stays_opt_out_through_the_config() {
        let keys = [INSTANT_INTERRUPT_KEY, "features.instant_interrupt"];
        assert!(!config_declares("[features]\ntool_search = true\n", &keys));
        assert!(config_declares("[features]\ninstant_interrupt = false\n", &keys));
        assert!(config_declares("features.instant_interrupt = false\n", &keys));
        assert!(!config_declares("[features]\n# instant_interrupt = false\n", &keys));

        let mut command = codex_command(Path::new("codex"));
        apply_instant_interrupt_override(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // The real Codex home decides, so only assert the pair stays together.
        if let Some(index) = args.iter().position(|arg| arg == INSTANT_INTERRUPT_OVERRIDE) {
            assert_eq!(args.get(index - 1).map(String::as_str), Some("-c"));
        } else {
            assert!(codex_config_declares(&keys));
        }
    }

    #[test]
    fn turning_the_feature_on_also_silences_the_warning_it_causes() {
        let keys = [UNSTABLE_WARNING_KEY];
        assert!(!config_declares("model = \"gpt-5\"\n", &keys));
        assert!(config_declares(
            "suppress_unstable_features_warning = false\n",
            &keys
        ));

        let mut command = codex_command(Path::new("codex"));
        apply_mcp_2026_protocol_override(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // The warning override rides along with the flag that triggers it.
        let carries_flag = args.iter().any(|arg| arg == MCP_2026_PROTOCOL_OVERRIDE);
        let carries_warning = args.iter().any(|arg| arg == UNSTABLE_WARNING_OVERRIDE);
        if carries_flag && !codex_config_declares_unstable_features_warning() {
            assert!(carries_warning);
        }
        if !carries_flag {
            assert!(!carries_warning);
        }
    }

    #[test]
    fn the_planning_tool_is_asked_for_unless_the_config_decided_it() {
        // Codex 0.152 made `tools.update_plan.enabled` default to false, and a
        // plugin section full of `enabled` must not read as that decision.
        assert!(!config_declares_update_plan_tool(
            "model = \"gpt-5\"\n[plugins.\"documents@runtime\"]\nenabled = true\n"
        ));
        assert!(config_declares_update_plan_tool(
            "tools.update_plan.enabled = false\n"
        ));
        assert!(config_declares_update_plan_tool(
            "[tools.update_plan]\nenabled = true\n"
        ));
        assert!(config_declares_update_plan_tool(
            "[tools]\nupdate_plan = { enabled = false }\n"
        ));
        assert!(!config_declares_update_plan_tool(
            "[tools]\nweb_search = { enabled = true }\n"
        ));
        // A commented-out line is not a decision.
        assert!(!config_declares_update_plan_tool(
            "[tools.update_plan]\n# enabled = true\n"
        ));

        let mut command = codex_command(Path::new("codex"));
        apply_update_plan_tool_override(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // The real Codex home decides, so only assert the pair stays together.
        if let Some(index) = args.iter().position(|arg| arg == UPDATE_PLAN_TOOL_OVERRIDE) {
            assert_eq!(args.get(index - 1).map(String::as_str), Some("-c"));
        } else {
            assert!(codex_config_declares_update_plan_tool());
        }
    }

    #[test]
    fn the_app_server_inherits_the_cli_originator() {
        let mut command = codex_command(Path::new("codex"));
        apply_originator_override(&mut command);

        let originator = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(ORIGINATOR_OVERRIDE_ENV))
            .and_then(|(_, value)| value);
        // Without this the connectors endpoints answer with a Cloudflare
        // challenge page instead of the app list.
        assert_eq!(
            originator,
            env::var_os(ORIGINATOR_OVERRIDE_ENV)
                .is_none()
                .then(|| std::ffi::OsStr::new("codex_cli_rs"))
        );
    }

    #[test]
    fn app_server_passes_the_devezcode_room_and_bridge_discovery_to_browser_mcp_only() {
        assert_eq!(
            devezcode_browser_overrides(Some("room-1")),
            vec![
                "mcp_servers.devez-browser.env.DEVEZCODE_ROOM_ID=\"room-1\"".to_string(),
                "mcp_servers.devez-browser.env_vars=[\"DEVEZCODE_BRIDGE_PIPE\",\"DEVEZCODE_BRIDGE_TOKEN\"]"
                    .to_string(),
            ]
        );
        assert!(devezcode_browser_overrides(None).is_empty());
    }

    #[test]
    fn the_room_override_only_applies_where_the_browser_mcp_is_declared() {
        assert!(config_declares_devez_browser(
            "model = \"x\"\n[mcp_servers.devez-browser]\ncommand = \"node\"\n"
        ));
        assert!(config_declares_devez_browser(
            "  [mcp_servers.\"devez-browser\"]  # bridge\n"
        ));
        assert!(config_declares_devez_browser(
            "[mcp_servers.'devez-browser']"
        ));
        assert!(!config_declares_devez_browser(
            "[mcp_servers.chrome-devtools]\ncommand = \"npx\"\n"
        ));
        assert!(!config_declares_devez_browser(
            "[mcp_servers.devez-browser.env]\nDEVEZCODE_ROOM_ID = \"room-1\"\n"
        ));
    }

    #[test]
    fn rpc_errors_drop_the_html_page_they_arrive_with() {
        let error = json!({
            "code": 403,
            "message": "app/list: failed to list apps:Request failed with status 403 Forbidden: <html>\n  <head>\n    <style>body{font-family:Arial}</style>\n  </head>\n</html>"
        });
        assert_eq!(
            format_rpc_error(&error),
            "app/list: failed to list apps:Request failed with status 403 Forbidden (403)"
        );

        // Plain messages are left exactly as they are.
        assert_eq!(
            format_rpc_error(&json!({ "message": "reload 거부" })),
            "reload 거부"
        );

        // A runaway single line is capped rather than filling the screen.
        let long = "x".repeat(500);
        let condensed = condense_error_message(&long);
        assert_eq!(condensed.chars().count(), 201, "{condensed}");
        assert!(condensed.ends_with('…'), "{condensed}");

        // A JSON response body reads as its inner message, trailing text and all.
        assert_eq!(
            condense_error_message(
                "API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"},\"request_id\":\"req_1\"} · Please run /login"
            ),
            "API Error: 401 OAuth token has expired. · Please run /login"
        );
        // A body without a message is kept as it came.
        assert_eq!(
            condense_error_message("HTTP 401: {\"code\": \"token_expired\"}"),
            "HTTP 401: {\"code\": \"token_expired\"}"
        );
        // A guide line keeps the error it explains, and nothing past that.
        assert_eq!(
            condense_error_message("Sign in again.\nworktree refused\nmore\n"),
            "Sign in again.\nworktree refused"
        );
    }

    #[tokio::test]
    async fn cloned_clients_share_request_ids() {
        let (outbound, mut messages) = mpsc::unbounded_channel();
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let client = AppServerClient {
            outbound: Arc::new(std::sync::Mutex::new(Some(outbound))),
            pending: pending.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            closed: watch::channel(false).0,
        };

        let first = tokio::spawn({
            let client = client.clone();
            async move { client.request("first", Value::Null).await }
        });
        let second = tokio::spawn(async move { client.request("second", Value::Null).await });

        let first_message = messages.recv().await.expect("first request");
        let second_message = messages.recv().await.expect("second request");
        let mut ids = [first_message["id"].as_u64(), second_message["id"].as_u64()];
        ids.sort_unstable();
        assert_eq!(ids, [Some(1), Some(2)]);

        let (events, _) = mpsc::unbounded_channel();
        for id in ids.into_iter().flatten() {
            route_message(
                json!({ "id": id, "result": { "id": id } }),
                &pending,
                &events,
            )
            .await;
        }
        assert_eq!(first.await.expect("first task").unwrap()["id"], 1);
        assert_eq!(second.await.expect("second task").unwrap()["id"], 2);
    }
}
