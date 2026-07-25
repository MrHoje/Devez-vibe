use std::{
    collections::{HashMap, VecDeque},
    env,
    path::Path,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
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

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;

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
    Closed(String),
}

pub struct AppServer {
    child: Child,
    outbound: Option<mpsc::UnboundedSender<Value>>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    pending: PendingMap,
    next_id: AtomicU64,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl AppServer {
    pub async fn spawn(codex_path: &Path) -> Result<Self> {
        let resolved_codex = resolve_command(codex_path);
        let mut command = codex_command(&resolved_codex);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "Codex app-server를 시작하지 못했습니다: {}",
                    resolved_codex.display()
                )
            })?;

        let stdin = child.stdin.take().context("app-server stdin 연결 실패")?;
        let stdout = child.stdout.take().context("app-server stdout 연결 실패")?;
        let stderr = child.stderr.take().context("app-server stderr 연결 실패")?;

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, events) = mpsc::unbounded_channel::<ServerEvent>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(20)));

        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = outbound_rx.recv().await {
                let mut encoded = match serde_json::to_vec(&message) {
                    Ok(encoded) => encoded,
                    Err(_) => continue,
                };
                encoded.push(b'\n');
                if stdin.write_all(&encoded).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

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
                                route_message(message, &reader_pending, &reader_events).await;
                            }
                            Err(error) => {
                                let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                                    "app-server JSON 해석 실패: {error}"
                                )));
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events.send(ServerEvent::ProtocolWarning(format!(
                            "app-server 출력 읽기 실패: {error}"
                        )));
                        break;
                    }
                }
            }

            let tail = reader_stderr.lock().await;
            let detail = if tail.is_empty() {
                "app-server 연결이 종료되었습니다.".to_owned()
            } else {
                format!(
                    "app-server 연결이 종료되었습니다.\n{}",
                    tail.iter().cloned().collect::<Vec<_>>().join("\n")
                )
            };
            drop(tail);

            let mut pending = reader_pending.lock().await;
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
            outbound: Some(outbound_tx),
            events,
            pending,
            next_id: AtomicU64::new(1),
            writer_task,
            reader_task,
            stderr_task,
        })
    }

    pub async fn initialize(&self) -> Result<Value> {
        let response = self.request("initialize", initialize_params()).await?;
        self.notify("initialized", None)?;
        Ok(response)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);

        let message = json!({
            "id": id,
            "method": method,
            "params": params
        });

        if let Err(error) = self.send(message) {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        match response_rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => bail!("{method}: {error}"),
            Err(_) => bail!("{method}: app-server 응답 채널이 종료되었습니다."),
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

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    pub async fn shutdown(mut self) {
        self.outbound.take();
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

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .as_ref()
            .ok_or_else(|| anyhow!("app-server 연결이 이미 종료되었습니다."))?
            .send(message)
            .map_err(|_| anyhow!("app-server에 메시지를 보낼 수 없습니다."))
    }
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "devez-cli",
            "title": "Devez CLI",
            "version": env!("CARGO_PKG_VERSION")
        },
        "capabilities": {
            "experimentalApi": true,
            "requestAttestation": false,
            "mcpServerOpenaiFormElicitation": true
        }
    })
}

fn codex_command(resolved: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = resolved
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            if let Some(root) = resolved.parent() {
                let script = root
                    .join("node_modules")
                    .join("@openai")
                    .join("codex")
                    .join("bin")
                    .join("codex.js");
                if script.is_file() {
                    let bundled_node = root.join("node.exe");
                    let node = if bundled_node.is_file() {
                        bundled_node
                    } else {
                        resolve_command(Path::new("node"))
                    };
                    let mut command = Command::new(node);
                    command
                        .arg(script)
                        .args(["app-server", "--listen", "stdio://"]);
                    return command;
                }
            }

            let shell = env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
            let mut command = Command::new(shell);
            command.args(["/d", "/s", "/c"]).arg(resolved).args([
                "app-server",
                "--listen",
                "stdio://",
            ]);
            return command;
        }
    }

    let mut command = Command::new(resolved);
    command.args(["app-server", "--listen", "stdio://"]);
    command
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

async fn route_message(
    message: Value,
    pending: &PendingMap,
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
        }
        return;
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let _ = events.send(ServerEvent::ProtocolWarning(
            "method 없는 app-server 메시지를 무시했습니다.".to_owned(),
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
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("알 수 없는 app-server 오류");
    match code {
        Some(code) => format!("{message} ({code})"),
        None => message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_enables_interactive_mcp_forms() {
        let params = initialize_params();

        assert_eq!(
            params
                .pointer("/capabilities/mcpServerOpenaiFormElicitation")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            params.pointer("/clientInfo/name").and_then(Value::as_str),
            Some("devez-cli")
        );
    }
}
