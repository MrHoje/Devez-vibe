use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    app_server::{AppServer, AppServerClient, ServerEvent},
    open_code::{OpenCodeServer, is_open_code_model, is_open_code_request_id},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeKind {
    Codex,
    OpenCode,
}

#[derive(Clone)]
struct Route {
    active: RuntimeKind,
    codex_id: Option<String>,
    open_code_id: Option<String>,
    cwd: PathBuf,
}

pub struct BackendServer {
    codex: AppServer,
    open_code: Option<OpenCodeServer>,
    routes: Arc<StdMutex<HashMap<String, Route>>>,
    aliases: Arc<StdMutex<HashMap<String, String>>>,
    cwd: PathBuf,
}

impl BackendServer {
    pub async fn spawn(codex_path: &Path, open_code_path: &Path, cwd: &Path) -> Result<Self> {
        let codex = AppServer::spawn(codex_path).await?;
        let open_code = OpenCodeServer::spawn(open_code_path, cwd).await.ok();
        Ok(Self {
            codex,
            open_code,
            routes: Arc::new(StdMutex::new(HashMap::new())),
            aliases: Arc::new(StdMutex::new(HashMap::new())),
            cwd: cwd.to_path_buf(),
        })
    }

    pub async fn initialize(&self) -> Result<Value> {
        let codex = self.codex.initialize().await?;
        if let Some(open_code) = &self.open_code {
            let _ = open_code.initialize().await;
        }
        Ok(codex)
    }

    pub async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        match method {
            "model/list" => {
                let mut response = self.codex.request(method, params).await?;
                if let Some(open_code) = &self.open_code
                    && let Ok(catalog) = open_code.model_catalog(&self.cwd).await
                    && let (Some(target), Some(extra)) = (
                        response.get_mut("data").and_then(Value::as_array_mut),
                        catalog.get("data").and_then(Value::as_array),
                    )
                {
                    target.extend(extra.iter().cloned());
                }
                Ok(response)
            }
            "thread/list" => {
                let mut response = self.codex.request(method, params.clone()).await?;
                if let Some(open_code) = &self.open_code {
                    let cwd = params.get("cwd").and_then(Value::as_str).map(Path::new);
                    if let Ok(extra) = open_code.list_sessions(cwd).await
                        && let (Some(target), Some(sessions)) = (
                            response.get_mut("data").and_then(Value::as_array_mut),
                            extra.get("data").and_then(Value::as_array),
                        )
                    {
                        for session in sessions {
                            if let Some(id) = session.get("id").and_then(Value::as_str) {
                                self.register_route(
                                    id,
                                    RuntimeKind::OpenCode,
                                    None,
                                    Some(id.to_owned()),
                                    session
                                        .get("cwd")
                                        .and_then(Value::as_str)
                                        .map(PathBuf::from)
                                        .unwrap_or_else(|| self.cwd.clone()),
                                );
                            }
                            target.push(session.clone());
                        }
                    }
                }
                Ok(response)
            }
            "thread/start" => {
                let model = params.get("model").and_then(Value::as_str);
                if model.is_some_and(is_open_code_model) {
                    let open_code = self.open_code()?;
                    let cwd = request_cwd(&params).unwrap_or_else(|| self.cwd.clone());
                    let response = open_code
                        .start_session(&cwd, model.expect("checked"))
                        .await?;
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .context("OpenCode thread/start 응답에 id가 없습니다.")?;
                    self.register_route(id, RuntimeKind::OpenCode, None, Some(id.to_owned()), cwd);
                    Ok(response)
                } else {
                    let response = self.codex.request(method, params).await?;
                    self.register_codex_response(&response);
                    Ok(response)
                }
            }
            "thread/resume" => {
                let visible = thread_id(&params)?;
                if self.is_open_code_thread(visible) {
                    let open_code = self.open_code()?;
                    let cwd = request_cwd(&params).unwrap_or_else(|| self.cwd.clone());
                    let response = open_code.resume_session(&cwd, visible).await?;
                    self.register_route(
                        visible,
                        RuntimeKind::OpenCode,
                        None,
                        Some(visible.to_owned()),
                        cwd,
                    );
                    Ok(response)
                } else {
                    let response = self.codex.request(method, params).await?;
                    self.register_codex_response(&response);
                    Ok(response)
                }
            }
            "thread/turns/list"
                if self.route_kind(thread_id(&params)?) == RuntimeKind::OpenCode =>
            {
                Ok(json!({ "data": [], "nextCursor": null }))
            }
            "turn/start" | "turn/steer" => {
                let visible = thread_id(&params)?.to_owned();
                let selected_open_code = selected_runtime(
                    params.get("model").and_then(Value::as_str),
                    self.route_kind(&visible),
                ) == RuntimeKind::OpenCode;
                if selected_open_code {
                    let (backing, model) = self.ensure_open_code_route(&visible, &params).await?;
                    if let Some(model) = model {
                        self.open_code()?
                            .set_model(
                                &backing,
                                &model,
                                params.get("effort").and_then(Value::as_str),
                            )
                            .await?;
                    }
                    let input = params
                        .get("input")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let instructions = params
                        .pointer("/additionalContext/devez-vibe-rules/value")
                        .and_then(Value::as_str);
                    let turn = self
                        .open_code()?
                        .start_prompt_content(&backing, input, instructions)
                        .await?;
                    Ok(json!({ "turn": { "id": turn } }))
                } else {
                    let backing = self.ensure_codex_route(&visible, &params).await?;
                    params["threadId"] = json!(backing);
                    self.codex.request(method, params).await
                }
            }
            "turn/interrupt" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let backing = self.backing_id(visible, RuntimeKind::OpenCode)?;
                    self.open_code()?.cancel(&backing)?;
                    Ok(json!({}))
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    self.codex.request(method, params).await
                }
            }
            "thread/fork" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let route = self.route(visible);
                    let backing = route
                        .as_ref()
                        .and_then(|route| route.open_code_id.as_deref())
                        .unwrap_or(visible);
                    let cwd = route
                        .as_ref()
                        .map(|route| route.cwd.clone())
                        .unwrap_or_else(|| self.cwd.clone());
                    let response = self.open_code()?.fork_session(&cwd, backing).await?;
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .context("OpenCode thread/fork 응답에 id가 없습니다.")?;
                    self.register_route(id, RuntimeKind::OpenCode, None, Some(id.to_owned()), cwd);
                    Ok(response)
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    let response = self.codex.request(method, params).await?;
                    self.register_codex_response(&response);
                    Ok(response)
                }
            }
            "thread/compact/start" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let backing = self.backing_id(visible, RuntimeKind::OpenCode)?;
                    let turn = self.open_code()?.start_prompt(&backing, "/compact").await?;
                    Ok(json!({ "turn": { "id": turn } }))
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    self.codex.request(method, params).await
                }
            }
            "thread/settings/update" | "thread/unsubscribe"
                if self.route_kind(thread_id(&params)?) == RuntimeKind::OpenCode =>
            {
                Ok(json!({}))
            }
            "config/value/write" if provider_default_write(&params)? => Ok(json!({})),
            _ => self.codex.request(method, params).await,
        }
    }

    pub fn client(&self) -> AppServerClient {
        self.codex.client()
    }

    pub async fn provider_catalog(&self) -> Result<Value> {
        self.open_code()?.provider_catalog().await
    }

    pub async fn set_provider_api_key(
        &self,
        provider_id: &str,
        key: &str,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        self.open_code()?
            .set_provider_api_key(provider_id, key, inputs)
            .await
    }

    pub async fn authorize_provider_oauth(
        &self,
        provider_id: &str,
        method: usize,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        self.open_code()?
            .authorize_provider_oauth(provider_id, method, inputs)
            .await
    }

    pub async fn complete_provider_oauth(
        &self,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<()> {
        self.open_code()?
            .complete_provider_oauth(provider_id, method, code)
            .await
    }

    pub fn has_open_code(&self) -> bool {
        self.open_code.is_some()
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        if is_open_code_request_id(&id) {
            self.open_code()?.respond(id, result)
        } else {
            self.codex.respond(id, result)
        }
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        if is_open_code_request_id(&id) {
            self.open_code()?.respond_error(id, code, message)
        } else {
            self.codex.respond_error(id, code, message)
        }
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        let (source, event) = if let Some(open_code) = self.open_code.as_mut() {
            tokio::select! {
                event = self.codex.next_event() => (RuntimeKind::Codex, event),
                event = open_code.next_event() => (RuntimeKind::OpenCode, event),
            }
        } else {
            (RuntimeKind::Codex, self.codex.next_event().await)
        };
        if source == RuntimeKind::OpenCode
            && (event.is_none() || matches!(event, Some(ServerEvent::Closed(_))))
        {
            let detail = match event {
                Some(ServerEvent::Closed(detail)) => detail,
                _ => "OpenCode ACP 이벤트 채널이 종료되었습니다.".to_owned(),
            };
            if let Some(open_code) = self.open_code.take() {
                tokio::spawn(open_code.shutdown());
            }
            return Some(ServerEvent::ProtocolWarning(detail));
        }
        event.map(|event| self.rewrite_event(event))
    }

    pub async fn shutdown(self) {
        let Self {
            codex, open_code, ..
        } = self;
        tokio::join!(codex.shutdown(), async move {
            if let Some(open_code) = open_code {
                open_code.shutdown().await;
            }
        });
    }

    fn open_code(&self) -> Result<&OpenCodeServer> {
        self.open_code
            .as_ref()
            .context("OpenCode가 설치되어 있지 않거나 ACP를 시작할 수 없습니다.")
    }

    fn register_codex_response(&self, response: &Value) {
        let Some(id) = response
            .get("id")
            .or_else(|| response.pointer("/thread/id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let cwd = response
            .get("cwd")
            .or_else(|| response.pointer("/thread/cwd"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone());
        self.register_route(id, RuntimeKind::Codex, Some(id.to_owned()), None, cwd);
    }

    fn register_route(
        &self,
        visible: &str,
        active: RuntimeKind,
        codex_id: Option<String>,
        open_code_id: Option<String>,
        cwd: PathBuf,
    ) {
        let mut routes = self.routes.lock().expect("routes mutex");
        let route = routes.entry(visible.to_owned()).or_insert(Route {
            active,
            codex_id: None,
            open_code_id: None,
            cwd: cwd.clone(),
        });
        route.active = active;
        route.cwd = cwd;
        if let Some(id) = codex_id {
            self.aliases
                .lock()
                .expect("aliases mutex")
                .insert(id.clone(), visible.to_owned());
            route.codex_id = Some(id);
        }
        if let Some(id) = open_code_id {
            self.aliases
                .lock()
                .expect("aliases mutex")
                .insert(id.clone(), visible.to_owned());
            route.open_code_id = Some(id);
        }
    }

    fn route(&self, visible: &str) -> Option<Route> {
        self.routes
            .lock()
            .expect("routes mutex")
            .get(visible)
            .cloned()
    }

    fn route_kind(&self, visible: &str) -> RuntimeKind {
        self.route(visible)
            .map(|route| route.active)
            .unwrap_or_else(|| {
                if visible.starts_with("ses_") {
                    RuntimeKind::OpenCode
                } else {
                    RuntimeKind::Codex
                }
            })
    }

    fn is_open_code_thread(&self, visible: &str) -> bool {
        self.route_kind(visible) == RuntimeKind::OpenCode
    }

    fn backing_id(&self, visible: &str, kind: RuntimeKind) -> Result<String> {
        let route = self.route(visible);
        let backing = match kind {
            RuntimeKind::Codex => route.and_then(|route| route.codex_id),
            RuntimeKind::OpenCode => route.and_then(|route| route.open_code_id),
        };
        backing
            .or_else(|| (self.route_kind(visible) == kind).then(|| visible.to_owned()))
            .with_context(|| format!("세션 `{visible}`의 런타임 연결을 찾을 수 없습니다."))
    }

    async fn ensure_open_code_route(
        &self,
        visible: &str,
        params: &Value,
    ) -> Result<(String, Option<String>)> {
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| is_open_code_model(model))
            .map(ToOwned::to_owned);
        if let Some(route) = self.route(visible)
            && let Some(backing) = route.open_code_id
        {
            self.register_route(
                visible,
                RuntimeKind::OpenCode,
                route.codex_id,
                Some(backing.clone()),
                route.cwd,
            );
            return Ok((backing, model));
        }
        let model = model.context("OpenCode 런타임으로 전환할 모델이 없습니다.")?;
        let cwd = self
            .route(visible)
            .map(|route| route.cwd)
            .or_else(|| request_cwd(params))
            .unwrap_or_else(|| self.cwd.clone());
        let response = self.open_code()?.start_session(&cwd, &model).await?;
        let backing = response
            .get("id")
            .and_then(Value::as_str)
            .context("OpenCode 전환 세션에 id가 없습니다.")?
            .to_owned();
        let codex_id = self.route(visible).and_then(|route| route.codex_id);
        self.register_route(
            visible,
            RuntimeKind::OpenCode,
            codex_id,
            Some(backing.clone()),
            cwd,
        );
        Ok((backing, Some(model)))
    }

    async fn ensure_codex_route(&self, visible: &str, params: &Value) -> Result<String> {
        if let Some(route) = self.route(visible)
            && let Some(backing) = route.codex_id
        {
            self.register_route(
                visible,
                RuntimeKind::Codex,
                Some(backing.clone()),
                route.open_code_id,
                route.cwd,
            );
            return Ok(backing);
        }
        let cwd = self
            .route(visible)
            .map(|route| route.cwd)
            .unwrap_or_else(|| self.cwd.clone());
        let model = params.get("model").and_then(Value::as_str);
        let response = self
            .codex
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "model": model,
                    "experimentalRawEvents": false,
                    "persistExtendedHistory": true
                }),
            )
            .await?;
        let backing = response
            .get("id")
            .or_else(|| response.pointer("/thread/id"))
            .and_then(Value::as_str)
            .context("Codex 전환 세션에 id가 없습니다.")?
            .to_owned();
        let open_code_id = self.route(visible).and_then(|route| route.open_code_id);
        self.register_route(
            visible,
            RuntimeKind::Codex,
            Some(backing.clone()),
            open_code_id,
            cwd,
        );
        Ok(backing)
    }

    fn rewrite_event(&self, mut event: ServerEvent) -> ServerEvent {
        match &mut event {
            ServerEvent::Notification { params, .. } | ServerEvent::Request { params, .. } => {
                if let Some(backing) = params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    && let Some(visible) = self
                        .aliases
                        .lock()
                        .expect("aliases mutex")
                        .get(&backing)
                        .cloned()
                {
                    params["threadId"] = json!(visible);
                }
            }
            _ => {}
        }
        event
    }
}

pub fn read_provider_config() -> String {
    provider_config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default()
}

fn provider_default_write(params: &Value) -> Result<bool> {
    let Some(key) = params.get("keyPath").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(value) = params.get("value").and_then(Value::as_str) else {
        return Ok(false);
    };
    if key == "model" {
        if is_open_code_model(value) {
            write_provider_config(value, "default")?;
            return Ok(true);
        }
        if let Some(path) = provider_config_path()
            && path.is_file()
        {
            fs::remove_file(path)?;
        }
        return Ok(false);
    }
    if key == "model_reasoning_effort" {
        let config = read_provider_config();
        if let Some(model) = root_config_value(&config, "model")
            && is_open_code_model(model)
        {
            write_provider_config(model, value)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn write_provider_config(model: &str, effort: &str) -> Result<()> {
    let path = provider_config_path().context("Devez Vibe 설정 경로를 찾을 수 없습니다.")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!(
            "model = {}\nmodel_reasoning_effort = {}\n",
            toml_string(model),
            toml_string(effort)
        ),
    )?;
    Ok(())
}

fn provider_config_path() -> Option<PathBuf> {
    if let Some(app_data) = env::var_os("APPDATA") {
        return Some(
            PathBuf::from(app_data)
                .join("DevezVibe")
                .join("provider.toml"),
        );
    }
    env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join(".config")
            .join("devez-vibe")
            .join("provider.toml")
    })
}

fn root_config_value<'a>(config: &'a str, name: &str) -> Option<&'a str> {
    config
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == name).then(|| value.trim().trim_matches(['"', '\'']))
        })
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn request_cwd(params: &Value) -> Option<PathBuf> {
    params.get("cwd").and_then(Value::as_str).map(PathBuf::from)
}

fn selected_runtime(model: Option<&str>, current: RuntimeKind) -> RuntimeKind {
    model.map_or(current, |model| {
        if is_open_code_model(model) {
            RuntimeKind::OpenCode
        } else {
            RuntimeKind::Codex
        }
    })
}

fn thread_id(params: &Value) -> Result<&str> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .context("요청에 threadId가 없습니다.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_model_switches_between_runtimes() {
        assert!(
            selected_runtime(Some("opencode:anthropic/claude"), RuntimeKind::Codex)
                == RuntimeKind::OpenCode
        );
        assert!(selected_runtime(Some("gpt-5.6-sol"), RuntimeKind::OpenCode) == RuntimeKind::Codex);
    }
}
