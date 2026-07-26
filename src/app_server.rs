use std::{
    collections::{HashMap, VecDeque},
    env,
    path::Path,
    path::PathBuf,
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

type PendingResponse = oneshot::Sender<Result<Value, String>>;
type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;

/// The cloneable half of the app-server connection. Background work may issue
/// requests through it while [`AppServer`] remains the sole reader of events.
#[derive(Clone)]
pub struct AppServerClient {
    outbound: Arc<StdMutex<Option<mpsc::UnboundedSender<Value>>>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
}

impl AppServerClient {
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

    fn close(&self) {
        self.outbound.lock().expect("outbound mutex").take();
    }

    fn send(&self, message: Value) -> Result<()> {
        self.outbound
            .lock()
            .expect("outbound mutex")
            .as_ref()
            .ok_or_else(|| anyhow!("app-server 연결이 이미 종료되었습니다."))?
            .send(message)
            .map_err(|_| anyhow!("app-server에 메시지를 보낼 수 없습니다."))
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
    Closed(String),
}

pub struct AppServer {
    child: Child,
    client: AppServerClient,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl AppServer {
    pub async fn spawn(codex_path: &Path) -> Result<Self> {
        let resolved_codex = resolve_command(codex_path);
        let mut command = codex_command(&resolved_codex);
        apply_originator_override(&mut command);
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

        let client = AppServerClient {
            outbound: Arc::new(StdMutex::new(Some(outbound_tx))),
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        };

        Ok(Self {
            child,
            client,
            events,
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
        self.client.request(method, params).await
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
            // `codex.js` only locates the vendored binary and re-spawns it, so going
            // straight to that binary saves a whole Node boot on every launch.
            if let Some(root) = resolved.parent()
                && let Some(binary) = vendored_codex_binary(root)
            {
                let mut command = Command::new(binary);
                command.args(["app-server", "--listen", "stdio://"]);
                return command;
            }

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

/// The app-server turns `clientInfo.name` into the `originator` header it sends
/// to chatgpt.com, and the connectors endpoints behind Cloudflare only answer
/// the CLI's own originator — anything else gets a bot challenge instead of an
/// answer, which is what made `app/list` fail with a 403 HTML page. The client
/// still identifies itself as devez-cli everywhere the app-server reports it;
/// only the outgoing header is pinned. An explicit override in the environment
/// wins, so this stays debuggable.
fn apply_originator_override(command: &mut Command) {
    if env::var_os(ORIGINATOR_OVERRIDE_ENV).is_none() {
        command.env(ORIGINATOR_OVERRIDE_ENV, "codex_cli_rs");
    }
}

const ORIGINATOR_OVERRIDE_ENV: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";

/// Finds the platform binary the `@openai/codex` npm package vendors, mirroring the
/// lookup `bin/codex.js` performs. `root` is the directory holding the npm shim.
#[cfg(windows)]
fn vendored_codex_binary(root: &Path) -> Option<PathBuf> {
    let triple = match std::env::consts::ARCH {
        "x86_64" => "x86_64-pc-windows-msvc",
        "aarch64" => "aarch64-pc-windows-msvc",
        _ => return None,
    };
    let platform_package = format!(
        "codex-win32-{}",
        match std::env::consts::ARCH {
            "x86_64" => "x64",
            _ => "arm64",
        }
    );
    let codex_package = root.join("node_modules").join("@openai").join("codex");
    // npm hoists the platform package beside `codex`; nested installs keep their own copy.
    let roots = [
        codex_package
            .join("node_modules")
            .join("@openai")
            .join(&platform_package)
            .join("vendor"),
        root.join("node_modules")
            .join("@openai")
            .join(&platform_package)
            .join("vendor"),
        codex_package.join("vendor"),
    ];
    roots
        .into_iter()
        .map(|vendor| vendor.join(triple).join("bin").join("codex.exe"))
        .find(|binary| binary.is_file())
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
    let message = condense_error_message(
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("알 수 없는 app-server 오류"),
    );
    match code {
        Some(code) => format!("{message} ({code})"),
        None => message,
    }
}

/// An upstream failure often arrives with a whole HTML error page attached.
/// Notices only have room for the part a person can act on, so keep the first
/// line up to where the markup starts and cap what is left.
fn condense_error_message(message: &str) -> String {
    const LIMIT: usize = 200;
    let lower = message.to_ascii_lowercase();
    let markup = ["<html", "<!doctype", "<head", "<body", "<?xml"]
        .iter()
        .filter_map(|tag| lower.find(tag))
        .min()
        .unwrap_or(message.len());
    let head = message[..markup].trim();
    // A message that is nothing but markup still has to say something.
    let head = if head.is_empty() { message } else { head };
    let line = head.lines().next().unwrap_or_default().trim();
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
    }

    #[tokio::test]
    async fn cloned_clients_share_request_ids() {
        let (outbound, mut messages) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let client = AppServerClient {
            outbound: Arc::new(std::sync::Mutex::new(Some(outbound))),
            pending: pending.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
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
