use std::{
    collections::HashMap,
    env, fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
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

use crate::app_server::ServerEvent;

/// OpenCode 연동 코드는 후속 개선을 위해 유지하되 현재 배포에서는 비활성화한다.
pub const PROVIDER_ENABLED: bool = false;

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;
type DetachedMap = Arc<Mutex<HashMap<u64, DetachedTurn>>>;
type ToolMap = Arc<Mutex<HashMap<String, Value>>>;

#[derive(Clone)]
struct DetachedTurn {
    session_id: String,
    turn_id: String,
}

#[derive(Clone)]
pub struct OpenCodeClient {
    outbound: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,
    pending: PendingMap,
    detached: DetachedMap,
    next_id: Arc<AtomicU64>,
    next_turn: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<ServerEvent>,
}

impl OpenCodeClient {
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
        match response_rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => bail!("{method}: {error}"),
            Err(_) => bail!("{method}: OpenCode ACP 응답 채널이 종료되었습니다."),
        }
    }

    pub async fn start_prompt(&self, session_id: &str, prompt: Vec<Value>) -> Result<String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let turn_id = format!(
            "opencode-turn-{}",
            self.next_turn.fetch_add(1, Ordering::Relaxed)
        );
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
            return Err(error);
        }
        let _ = self.events.send(ServerEvent::Notification {
            method: "turn/started".to_owned(),
            params: json!({
                "threadId": session_id,
                "turn": { "id": turn_id }
            }),
        });
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
        let id = open_code_request_id(&id)?;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": permission_result(&result)
        }))
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        let id = open_code_request_id(&id)?;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }))
    }

    fn close(&self) {
        self.outbound.lock().expect("outbound mutex").take();
    }

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .lock()
            .expect("outbound mutex")
            .as_ref()
            .ok_or_else(|| anyhow!("OpenCode ACP 연결이 이미 종료되었습니다."))?
            .send(message)
            .map_err(|_| anyhow!("OpenCode ACP에 메시지를 보낼 수 없습니다."))
    }
}

pub struct OpenCodeServer {
    child: Child,
    client: OpenCodeClient,
    provider_auth: ProviderAuthServer,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

pub struct ProviderAuthServer {
    client: reqwest::Client,
    base_url: reqwest::Url,
}

impl ProviderAuthServer {
    fn new(port: u16) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            base_url: reqwest::Url::parse(&format!("http://127.0.0.1:{port}/"))?,
        })
    }

    async fn wait_until_ready(&self) -> Result<()> {
        for _ in 0..60 {
            if self
                .client
                .get(self.url(&["global", "health"])?)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!("OpenCode provider API가 제한 시간 안에 준비되지 않았습니다.")
    }

    pub async fn catalog(&self) -> Result<Value> {
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

    pub async fn set_api_key(
        &self,
        provider_id: &str,
        key: &str,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
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
            .context("OpenCode provider API 요청 실패")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
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
            serde_json::from_str(&body).context("OpenCode provider API 응답 해석 실패")
        }
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow!("OpenCode provider API URL을 만들 수 없습니다."))?
            .extend(segments);
        Ok(url)
    }
}

impl OpenCodeServer {
    pub async fn spawn(open_code_path: &Path, cwd: &Path) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("OpenCode provider API 포트를 확보하지 못했습니다.")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let resolved = resolve_command(open_code_path);
        let mut command = command_for(&resolved);
        isolate_ctrl_c(&mut command);
        command
            .args([
                "acp",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--cwd",
            ])
            .arg(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            format!("OpenCode ACP를 시작하지 못했습니다: {}", resolved.display())
        })?;
        let stdin = child.stdin.take().context("OpenCode ACP stdin 연결 실패")?;
        let stdout = child
            .stdout
            .take()
            .context("OpenCode ACP stdout 연결 실패")?;
        let stderr = child
            .stderr
            .take()
            .context("OpenCode ACP stderr 연결 실패")?;

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, events) = mpsc::unbounded_channel::<ServerEvent>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let detached: DetachedMap = Arc::new(Mutex::new(HashMap::new()));
        let tools: ToolMap = Arc::new(Mutex::new(HashMap::new()));

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
        let reader_detached = Arc::clone(&detached);
        let reader_tools = Arc::clone(&tools);
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
                                    &reader_tools,
                                    &reader_events,
                                )
                                .await;
                            }
                            Err(error) => {
                                let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                                    "OpenCode ACP JSON 해석 실패: {error}"
                                )));
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                            "OpenCode ACP 출력 읽기 실패: {error}"
                        )));
                        break;
                    }
                }
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("OpenCode ACP 연결이 종료되었습니다.".to_owned()));
            }
            let _ = reader_events.send(ServerEvent::Closed(
                "OpenCode ACP 연결이 종료되었습니다.".to_owned(),
            ));
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

        let client = OpenCodeClient {
            outbound: Arc::new(StdMutex::new(Some(outbound_tx))),
            pending,
            detached,
            next_id: Arc::new(AtomicU64::new(1)),
            next_turn: Arc::new(AtomicU64::new(1)),
            events: event_tx,
        };
        Ok(Self {
            child,
            client,
            provider_auth: ProviderAuthServer::new(port)?,
            events,
            writer_task,
            reader_task,
            stderr_task,
        })
    }

    pub async fn initialize(&self) -> Result<Value> {
        self.client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false,
                        "_meta": { "terminal-auth": true }
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

    pub async fn model_catalog(&self, cwd: &Path) -> Result<Value> {
        let response = self
            .client
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .context("OpenCode 모델 조회 세션에 id가 없습니다.")?;
        let connected = connected_provider_ids();
        let models = model_options(&response)
            .into_iter()
            .filter_map(|(value, name)| {
                let provider = value.split_once('/')?.0;
                (provider != "openai"
                    && (connected.is_empty() || connected.iter().any(|id| id == provider)))
                .then(|| open_code_model(&value, &name))
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

    pub async fn start_session(&self, cwd: &Path, model: &str) -> Result<Value> {
        let response = self
            .client
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .context("OpenCode 새 세션에 id가 없습니다.")?;
        let model = strip_model_prefix(model);
        self.client
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": session_id,
                    "configId": "model",
                    "value": model
                }),
            )
            .await?;
        Ok(thread_response(
            session_id,
            cwd,
            &format!("opencode:{model}"),
        ))
    }

    pub async fn resume_session(&self, cwd: &Path, session_id: &str) -> Result<Value> {
        let response = self
            .client
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": cwd,
                    "mcpServers": []
                }),
            )
            .await?;
        let model = current_model(&response)
            .map(|model| format!("opencode:{model}"))
            .unwrap_or_else(|| "opencode:unknown/unknown".to_owned());
        Ok(thread_response(session_id, cwd, &model))
    }

    pub async fn list_sessions(&self, cwd: Option<&Path>) -> Result<Value> {
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        let response = self.client.request("session/list", params).await?;
        let data = response
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
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

    pub async fn fork_session(&self, cwd: &Path, session_id: &str) -> Result<Value> {
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
            .context("OpenCode 분기 세션에 id가 없습니다.")?;
        let model = current_model(&response)
            .map(|model| format!("opencode:{model}"))
            .unwrap_or_else(|| "opencode:unknown/unknown".to_owned());
        Ok(thread_response(forked, cwd, &model))
    }

    pub async fn start_prompt(&self, session_id: &str, text: &str) -> Result<String> {
        self.client
            .start_prompt(session_id, vec![json!({ "type": "text", "text": text })])
            .await
    }

    pub async fn start_prompt_content(
        &self,
        session_id: &str,
        input: &[Value],
        instructions: Option<&str>,
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
                        .context("이미지 입력에 경로가 없습니다.")?;
                    let data = fs::read(path)
                        .with_context(|| format!("이미지를 읽지 못했습니다: {path}"))?;
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
        self.client.start_prompt(session_id, prompt).await
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
    }
}

async fn route_message(
    message: Value,
    pending: &PendingMap,
    detached: &DetachedMap,
    tools: &ToolMap,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
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
        if let Some(turn) = detached.lock().await.remove(&id) {
            if let Some(error) = message.get("error") {
                let detail = format_rpc_error(error);
                let _ = events.send(ServerEvent::Notification {
                    method: "error".to_owned(),
                    params: json!({
                        "threadId": turn.session_id,
                        "error": { "message": detail },
                        "willRetry": false,
                        "provider": "OpenCode"
                    }),
                });
                let _ = events.send(ServerEvent::Notification {
                    method: "turn/completed".to_owned(),
                    params: json!({
                        "threadId": turn.session_id,
                        "turn": {
                            "id": turn.turn_id,
                            "error": { "message": detail }
                        }
                    }),
                });
            } else {
                let _ = events.send(ServerEvent::Notification {
                    method: "turn/completed".to_owned(),
                    params: json!({
                        "threadId": turn.session_id,
                        "turn": { "id": turn.turn_id }
                    }),
                });
            }
            return;
        }
    }

    if message.get("method").and_then(Value::as_str) == Some("session/update") {
        route_session_update(
            message.get("params").cloned().unwrap_or(Value::Null),
            tools,
            events,
        )
        .await;
        return;
    }

    if let (Some(id), Some(method)) = (
        message.get("id").cloned(),
        message.get("method").and_then(Value::as_str),
    ) && method == "session/request_permission"
    {
        let params =
            permission_request_params(message.get("params").cloned().unwrap_or(Value::Null));
        let _ = events.send(ServerEvent::Request {
            id: json!({ "backend": "opencode", "id": id }),
            method: params.0,
            params: params.1,
        });
    }
}

async fn route_session_update(
    params: Value,
    tools: &ToolMap,
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
            if let Some(delta) = content_text(update.get("content")) {
                let item_id = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("opencode-agent-message");
                notify(
                    events,
                    "item/agentMessage/delta",
                    json!({
                        "threadId": session_id,
                        "itemId": item_id,
                        "delta": delta,
                        "provider": "OpenCode"
                    }),
                );
            }
        }
        Some("agent_thought_chunk") => {
            if let Some(delta) = content_text(update.get("content")) {
                let item_id = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or("opencode-reasoning");
                notify(
                    events,
                    "item/reasoning/summaryTextDelta",
                    json!({
                        "threadId": session_id,
                        "itemId": item_id,
                        "delta": delta
                    }),
                );
            }
        }
        Some("tool_call") => {
            let item = tool_item(update, false);
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                tools.lock().await.insert(id.to_owned(), update.clone());
            }
            emit_plan(update, session_id, events);
            notify(
                events,
                "item/started",
                json!({ "threadId": session_id, "item": item }),
            );
        }
        Some("tool_call_update") => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("opencode-tool");
            let previous = tools.lock().await.get(id).cloned();
            let merged = merge_tool_update(previous, update);
            emit_plan(&merged, session_id, events);
            match update.get("status").and_then(Value::as_str) {
                Some("completed" | "failed") => {
                    tools.lock().await.remove(id);
                    notify(
                        events,
                        "item/completed",
                        json!({ "threadId": session_id, "item": tool_item(&merged, true) }),
                    );
                }
                _ => {
                    tools.lock().await.insert(id.to_owned(), merged);
                }
            }
        }
        Some("plan") => {
            let plan = update
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| {
                    json!({
                        "step": entry.get("content")
                            .or_else(|| entry.get("text"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "status": plan_status(entry.get("status").and_then(Value::as_str))
                    })
                })
                .collect::<Vec<_>>();
            notify(
                events,
                "turn/plan/updated",
                json!({ "threadId": session_id, "plan": plan }),
            );
        }
        Some("usage_update") => {
            let used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
            let size = update.get("size").and_then(Value::as_u64);
            notify(
                events,
                "thread/tokenUsage/updated",
                json!({
                    "threadId": session_id,
                    "tokenUsage": {
                        "last": { "totalTokens": used },
                        "total": { "totalTokens": used },
                        "modelContextWindow": size
                    }
                }),
            );
        }
        _ => {}
    }
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
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_plan_input = update
        .get("rawInput")
        .is_some_and(|input| input.get("todos").is_some() || input.get("plan").is_some());
    if !has_plan_input && title != "todowrite" && title != "update_plan" && title != "update plan" {
        return;
    }
    let Some(todos) = update
        .get("rawInput")
        .and_then(|input| input.get("todos").or_else(|| input.get("plan")))
        .and_then(Value::as_array)
    else {
        return;
    };
    let plan = todos
        .iter()
        .filter_map(|todo| {
            Some(json!({
                "step": todo.get("content")
                    .or_else(|| todo.get("step"))
                    .and_then(Value::as_str)?,
                "status": plan_status(todo.get("status").and_then(Value::as_str))
            }))
        })
        .collect::<Vec<_>>();
    notify(
        events,
        "turn/plan/updated",
        json!({ "threadId": session_id, "plan": plan }),
    );
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
        .context("OpenCode 요청 id가 올바르지 않습니다.")
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
    response
        .get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("model"))?
        .get("currentValue")?
        .as_str()
}

fn open_code_model(model: &str, display_name: &str) -> Value {
    json!({
        "id": format!("opencode:{model}"),
        "model": format!("opencode:{model}"),
        "displayName": format!("OpenCode · {display_name}"),
        "defaultReasoningEffort": "default",
        "supportedReasoningEfforts": [{ "reasoningEffort": "default" }],
        "isDefault": false
    })
}

fn thread_response(session_id: &str, cwd: &Path, model: &str) -> Value {
    json!({
        "id": session_id,
        "thread": {
            "id": session_id,
            "cwd": cwd,
            "turns": []
        },
        "cwd": cwd,
        "model": model,
        "reasoningEffort": "default"
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
        .unwrap_or("알 수 없는 OpenCode ACP 오류");
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

#[cfg(windows)]
fn isolate_ctrl_c(command: &mut Command) {
    command.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
}

#[cfg(not(windows))]
fn isolate_ctrl_c(_: &mut Command) {}

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

    #[test]
    fn open_code_models_are_namespaced() {
        let model = open_code_model("anthropic/claude-sonnet", "Anthropic/Claude Sonnet");
        assert_eq!(
            model.get("model").and_then(Value::as_str),
            Some("opencode:anthropic/claude-sonnet")
        );
        assert!(is_open_code_model("opencode:anthropic/claude-sonnet"));
        assert!(!is_open_code_model("gpt-5.6-sol"));
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
