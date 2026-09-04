use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex as StdMutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app_server::{AppServer, AppServerClient, ServerEvent},
    claude::{
        ClaudeClient, ClaudeServer, is_claude_model, is_claude_request_id, is_claude_thread,
        raw_thread_id, visible_thread_id,
    },
    open_code::{OpenCodeServer, is_open_code_model, is_open_code_request_id},
};

const CLAUDE_PREFERRED_PERMISSION_MODE: &str = "bypassPermissions";

#[derive(Clone)]
pub enum IntegrationClient {
    Codex(AppServerClient),
    Claude(ClaudeClient),
}

impl IntegrationClient {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Codex(client) => client.request(method, params).await,
            Self::Claude(client) => client.request(method, params).await,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum RuntimeKind {
    Codex,
    OpenCode,
    Claude,
}

impl RuntimeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Claude => "Claude",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Route {
    active: RuntimeKind,
    codex_id: Option<String>,
    open_code_id: Option<String>,
    claude_id: Option<String>,
    cwd: PathBuf,
    codex_seen_through: u64,
    open_code_seen_through: u64,
    claude_seen_through: u64,
    /// The model and effort this thread's Claude turns last ran on. The SDK
    /// transcript records neither — it names the resolved model, not the id the
    /// picker uses, and never the effort — so a resumed session would otherwise
    /// reopen on the launch defaults no matter what it was running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_effort: Option<String>,
    /// OpenCode's ACP session/load response reports the workspace defaults, not
    /// the selection the loaded session last ran on. Keep that selection beside
    /// the route so resume can restore both the runtime and the picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_code_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_code_effort: Option<String>,
    /// Monotonic-enough wall-clock stamp used when several dvz processes merge
    /// their route maps. Older route files deserialize this as zero.
    #[serde(default)]
    updated_at: u64,
}

impl Route {
    fn backing_count(&self) -> usize {
        [
            self.codex_id.is_some(),
            self.open_code_id.is_some(),
            self.claude_id.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    /// Worth keeping across launches when the thread's own id may not name the session
    /// that resumes it: a thread that mixed runtimes, or a Claude-backed one, whose id
    /// the CLI can rotate while the thread keeps the id it was announced under.
    fn is_worth_storing(&self) -> bool {
        self.backing_count() > 1 || self.claude_id.is_some() || self.open_code_model.is_some()
    }

    fn has_claude_codex_backings(&self) -> bool {
        self.claude_id.is_some() && self.codex_id.is_some()
    }

    fn touch(&mut self) {
        self.updated_at = store_timestamp();
    }

    fn seen_through(&self, kind: RuntimeKind) -> u64 {
        match kind {
            RuntimeKind::Codex => self.codex_seen_through,
            RuntimeKind::OpenCode => self.open_code_seen_through,
            RuntimeKind::Claude => self.claude_seen_through,
        }
    }

    fn note_seen_through(&mut self, kind: RuntimeKind, block_id: u64) {
        let seen = match kind {
            RuntimeKind::Codex => &mut self.codex_seen_through,
            RuntimeKind::OpenCode => &mut self.open_code_seen_through,
            RuntimeKind::Claude => &mut self.claude_seen_through,
        };
        *seen = (*seen).max(block_id);
    }
}

pub struct BackendServer {
    codex: Option<AppServer>,
    codex_unavailable_reason: Option<String>,
    open_code: Option<OpenCodeServer>,
    claude: ClaudeServer,
    codex_path: PathBuf,
    open_code_path: PathBuf,
    routes: Arc<StdMutex<HashMap<String, Route>>>,
    aliases: Arc<StdMutex<HashMap<String, String>>>,
    /// visible thread → the id that names the session a later `-r` has to resume.
    /// It equals the visible id until a thread outlives the runtime it was named
    /// after (a Claude-named room whose turns now run on Codex, a Claude session the
    /// CLI persisted under a rotated uuid).
    resume_ids: Arc<StdMutex<HashMap<String, String>>>,
    /// Rebind notices raised while answering a request, drained by `next_event`.
    pending_events: Arc<StdMutex<VecDeque<ServerEvent>>>,
    route_store_path: Option<PathBuf>,
    cwd: PathBuf,
}

impl BackendServer {
    pub async fn spawn(
        codex_path: &Path,
        open_code_path: &Path,
        node_path: &Path,
        claude_path: &Path,
        cwd: &Path,
    ) -> Result<Self> {
        let open_code = if crate::open_code::PROVIDER_ENABLED && open_code_is_startup_default() {
            OpenCodeServer::spawn(open_code_path, cwd).await.ok()
        } else {
            None
        };
        let claude = ClaudeServer::new(node_path, claude_path, cwd)?;
        let route_store_path = route_store_path();
        let routes = route_store_path
            .as_deref()
            .map(load_routes)
            .unwrap_or_default();
        let aliases = route_aliases(&routes);
        let resume_ids = route_resume_ids(&routes);
        Ok(Self {
            codex: None,
            codex_unavailable_reason: None,
            open_code,
            claude,
            codex_path: codex_path.to_path_buf(),
            open_code_path: open_code_path.to_path_buf(),
            routes: Arc::new(StdMutex::new(routes)),
            aliases: Arc::new(StdMutex::new(aliases)),
            resume_ids: Arc::new(StdMutex::new(resume_ids)),
            pending_events: Arc::new(StdMutex::new(VecDeque::new())),
            route_store_path,
            cwd: cwd.to_path_buf(),
        })
    }

    pub async fn initialize(&mut self) -> Result<()> {
        if let Some(codex) = self.codex.as_ref()
            && let Err(error) = codex.initialize().await
        {
            self.codex_unavailable_reason = Some(error.to_string());
            if let Some(codex) = self.codex.take() {
                codex.shutdown().await;
            }
        }
        if let Some(open_code) = &self.open_code {
            let _ = open_code.initialize().await;
        }
        Ok(())
    }

    pub fn has_codex(&self) -> bool {
        self.codex.is_some()
    }

    pub fn codex_unavailable_reason(&self) -> Option<&str> {
        self.codex_unavailable_reason.as_deref()
    }

    pub async fn start_codex(&mut self) -> Result<()> {
        if self.codex.is_some() {
            return Ok(());
        }
        // A PC that cannot reach the app-server turns the connection off in
        // `/provider`; every path into Codex — launch, resume, switch — stops
        // here rather than waiting out a spawn that will never answer.
        if !crate::state::codex_provider_enabled() {
            let reason =
                "Codex provider 연결이 꺼져 있습니다. /provider에서 Codex를 켜세요.".to_owned();
            self.codex_unavailable_reason = Some(reason.clone());
            anyhow::bail!(reason);
        }
        let devezcode_room = crate::devezcode::room_id();
        let codex = match AppServer::spawn(&self.codex_path, devezcode_room.as_deref()).await {
            Ok(codex) => codex,
            Err(error) => {
                self.codex_unavailable_reason = Some(error.to_string());
                return Err(error);
            }
        };
        if let Err(error) = codex.initialize().await {
            self.codex_unavailable_reason = Some(error.to_string());
            codex.shutdown().await;
            return Err(error);
        }
        self.codex_unavailable_reason = None;
        self.codex = Some(codex);
        Ok(())
    }

    pub async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        match method {
            "model/list" => {
                let mut response = if let Some(codex) = self.codex.as_ref() {
                    codex.request(method, params).await?
                } else {
                    empty_list_response()
                };
                if let Some(open_code) = &self.open_code
                    && let Ok(catalog) = open_code.model_catalog(&self.cwd).await
                    && let (Some(target), Some(extra)) = (
                        response.get_mut("data").and_then(Value::as_array_mut),
                        catalog.get("data").and_then(Value::as_array),
                    )
                {
                    target.extend(extra.iter().cloned());
                }
                let claude_catalog = self
                    .claude
                    .request("model/list", json!({ "cwd": self.cwd.to_string_lossy() }))
                    .await
                    .unwrap_or_else(|_| crate::claude::model_catalog());
                if let (Some(target), Some(extra)) = (
                    response.get_mut("data").and_then(Value::as_array_mut),
                    claude_catalog.get("data").and_then(Value::as_array),
                ) {
                    target.extend(extra.iter().cloned());
                }
                Ok(response)
            }
            "thread/list" => {
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if let Some(object) = params.as_object_mut() {
                    object.remove("provider");
                }
                let include =
                    |candidate: &str| provider.as_deref().is_none_or(|value| value == candidate);
                let mut response = if include("codex")
                    && let Some(codex) = self.codex.as_ref()
                {
                    codex.request(method, params.clone()).await?
                } else {
                    empty_list_response()
                };
                if let Some(sessions) = response.get_mut("data").and_then(Value::as_array_mut) {
                    let source = std::mem::take(sessions);
                    for mut session in source {
                        if let Some(backing) = session
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                        {
                            let visible = self.visible_id(&backing, &backing);
                            session["id"] = json!(visible);
                            self.register_discovered_route(
                                &visible,
                                RuntimeKind::Codex,
                                &backing,
                                session_cwd(&session, &self.cwd),
                            );
                        }
                        merge_session(sessions, session);
                    }
                }
                if include("opencode")
                    && let Some(open_code) = &self.open_code
                {
                    let cwd = params.get("cwd").and_then(Value::as_str).map(Path::new);
                    if let Ok(extra) = open_code.list_sessions(cwd).await
                        && let (Some(target), Some(sessions)) = (
                            response.get_mut("data").and_then(Value::as_array_mut),
                            extra.get("data").and_then(Value::as_array),
                        )
                    {
                        for session in sessions {
                            if let Some(id) = session.get("id").and_then(Value::as_str) {
                                let visible = self.visible_id(id, id);
                                let mut session = session.clone();
                                session["id"] = json!(visible);
                                self.register_discovered_route(
                                    &visible,
                                    RuntimeKind::OpenCode,
                                    id,
                                    session_cwd(&session, &self.cwd),
                                );
                                merge_session(target, session);
                            } else {
                                merge_session(target, session.clone());
                            }
                        }
                    }
                }
                let cwd = params.get("cwd").and_then(Value::as_str).map(Path::new);
                if include("claude")
                    && let Ok(extra) = self
                        .claude
                        .request(
                            "session/list",
                            json!({
                                "cwd": cwd,
                                "limit": params.get("limit").and_then(Value::as_u64).unwrap_or(100)
                            }),
                        )
                        .await
                    && let (Some(target), Some(sessions)) = (
                        response.get_mut("data").and_then(Value::as_array_mut),
                        extra.get("data").and_then(Value::as_array),
                    )
                {
                    for source in sessions {
                        let mut session = source.clone();
                        if let Some(namespaced) = source.get("id").and_then(Value::as_str) {
                            let backing = raw_thread_id(namespaced).to_owned();
                            let visible = self.visible_id(&backing, namespaced);
                            session["id"] = json!(visible);
                            self.register_discovered_route(
                                &visible,
                                RuntimeKind::Claude,
                                &backing,
                                session_cwd(source, &self.cwd),
                            );
                        }
                        merge_session(target, session);
                    }
                }
                if let Some(target) = response.get_mut("data").and_then(Value::as_array_mut) {
                    target.sort_by_key(|session| std::cmp::Reverse(session_updated_at(session)));
                    target.truncate(
                        params.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize
                    );
                }
                Ok(response)
            }
            "thread/start" => {
                let model = params.get("model").and_then(Value::as_str);
                if model.is_some_and(is_claude_model) {
                    let cwd = request_cwd(&params).unwrap_or_else(|| self.cwd.clone());
                    let mut response = self
                        .claude
                        .request("session/start", claude_session_params(&params, &cwd, None))
                        .await?;
                    self.register_claude_response(&mut response, cwd)?;
                    Ok(response)
                } else if model.is_some_and(is_open_code_model) {
                    let open_code = self.open_code()?;
                    let cwd = request_cwd(&params).unwrap_or_else(|| self.cwd.clone());
                    let response = open_code
                        .start_session(
                            &cwd,
                            model.expect("checked"),
                            params.get("effort").and_then(Value::as_str),
                        )
                        .await?;
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .context("OpenCode thread/start 응답에 id가 없습니다.")?;
                    self.register_route(
                        id,
                        RuntimeKind::OpenCode,
                        None,
                        Some(id.to_owned()),
                        None,
                        cwd,
                    );
                    self.note_open_code_selection(
                        id,
                        model,
                        params.get("effort").and_then(Value::as_str),
                    );
                    Ok(response)
                } else {
                    let response = self.codex()?.request(method, params).await?;
                    self.register_codex_response(&response);
                    Ok(response)
                }
            }
            "thread/resume" => {
                let visible = self.canonical_visible(thread_id(&params)?);
                if self.route_kind(&visible) == RuntimeKind::Claude {
                    let backing = self
                        .backing_id(&visible, RuntimeKind::Claude)
                        .unwrap_or_else(|_| raw_thread_id(&visible).to_owned());
                    let route = self.route(&visible);
                    let cwd = request_cwd(&params)
                        .or_else(|| route.as_ref().map(|route| route.cwd.clone()))
                        .unwrap_or_else(|| self.cwd.clone());
                    apply_remembered_claude_selection(&mut params, route.as_ref());
                    let mut response = self
                        .claude
                        .request(
                            "session/resume",
                            claude_session_params(&params, &cwd, Some(&backing)),
                        )
                        .await?;
                    self.register_claude_response_as(&mut response, cwd, Some(&visible))?;
                    Ok(response)
                } else if self.is_open_code_thread(&visible) {
                    let open_code = self.open_code()?;
                    let route = self.route(&visible);
                    let cwd = request_cwd(&params)
                        .or_else(|| route.as_ref().map(|route| route.cwd.clone()))
                        .unwrap_or_else(|| self.cwd.clone());
                    apply_remembered_open_code_selection(&mut params, route.as_ref());
                    let backing = self.backing_id(&visible, RuntimeKind::OpenCode)?;
                    let mut response = open_code.resume_session(&cwd, &backing).await?;
                    let model = params
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|model| is_open_code_model(model))
                        .map(ToOwned::to_owned);
                    let effort = params
                        .get("effort")
                        .and_then(Value::as_str)
                        .filter(|effort| !effort.is_empty())
                        .map(ToOwned::to_owned);
                    if let Some(model) = model.as_deref() {
                        open_code
                            .set_model(&backing, model, effort.as_deref())
                            .await?;
                        response["model"] = json!(model);
                        if let Some(effort) = effort.as_deref() {
                            response["reasoningEffort"] = json!(effort);
                        }
                    }
                    response["id"] = json!(visible);
                    self.register_route(
                        &visible,
                        RuntimeKind::OpenCode,
                        self.route(&visible).and_then(|route| route.codex_id),
                        Some(backing),
                        self.route(&visible).and_then(|route| route.claude_id),
                        cwd,
                    );
                    Ok(response)
                } else {
                    let backing = self.backing_id(&visible, RuntimeKind::Codex)?;
                    params["threadId"] = json!(backing);
                    let response = self.codex()?.request(method, params).await?;
                    Ok(self.register_codex_response_as(response, &visible))
                }
            }
            "thread/turns/list"
                if self.is_mixed_provider_route(&self.canonical_visible(thread_id(&params)?)) =>
            {
                let visible = self.canonical_visible(thread_id(&params)?);
                if let Some(history) = load_provider_handoff(&visible) {
                    return Ok(provider_handoff_history_page(&history));
                }
                self.mixed_provider_history(&visible, &params).await
            }
            "thread/turns/list"
                if self.route_kind(thread_id(&params)?) == RuntimeKind::OpenCode =>
            {
                let visible = self.canonical_visible(thread_id(&params)?);
                if let Some(page) = load_provider_handoff(&visible)
                    .as_ref()
                    .map(provider_handoff_history_page)
                {
                    return Ok(page);
                }
                // 사이드카가 없으면 session/load가 재생해 준 내역으로 채운다.
                // opencode CLI에서 만든 세션도 이 경로로 복원된다.
                let backing = self
                    .backing_id(&visible, RuntimeKind::OpenCode)
                    .unwrap_or_else(|_| visible.clone());
                let page = match self.open_code() {
                    Ok(open_code) => open_code.history_page(&backing).await,
                    Err(_) => None,
                };
                Ok(page.unwrap_or_else(|| json!({ "data": [], "nextCursor": null })))
            }
            "thread/turns/list" if self.route_kind(thread_id(&params)?) == RuntimeKind::Claude => {
                let visible = self.canonical_visible(thread_id(&params)?);
                let route = self.route(&visible);
                let backing = self.backing_id(&visible, RuntimeKind::Claude)?;
                self.claude
                    .request(
                        "session/history",
                        json!({
                            "sessionId": backing,
                            "cwd": route.map(|route| route.cwd).unwrap_or_else(|| self.cwd.clone())
                        }),
                    )
                    .await
            }
            "thread/turns/list" => {
                let visible = self.canonical_visible(thread_id(&params)?);
                params["threadId"] = json!(self.backing_id(&visible, RuntimeKind::Codex)?);
                self.codex()?.request(method, params).await
            }
            "turn/start" | "turn/steer" => {
                let visible = self.canonical_visible(thread_id(&params)?);
                let previous = self.route_kind(&visible);
                let selected =
                    selected_runtime(params.get("model").and_then(Value::as_str), previous);
                let snapshot = take_provider_handoff(&mut params);
                let switching = method == "turn/start" && selected != previous;
                let seen_through = self
                    .route(&visible)
                    .map(|route| route.seen_through(selected))
                    .unwrap_or_default();
                let handoff_context = switching
                    .then(|| {
                        snapshot.as_ref().and_then(|snapshot| {
                            snapshot.context_since(seen_through, previous, selected)
                        })
                    })
                    .flatten();
                if let Some(context) = handoff_context.as_deref() {
                    insert_handoff_context(&mut params, context);
                }
                let turn_context = combined_turn_instructions(&params, selected);

                // Read before the request: the Codex branch consumes `params`.
                let turn_model = params
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let turn_effort = params
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let response: Result<Value> = async {
                    if selected == RuntimeKind::Claude {
                        let backing = self.ensure_claude_route(&visible, &params).await?;
                        // Steering joins the turn already running, so it carries no
                        // model or permission change of its own.
                        if method == "turn/steer" {
                            return self
                                .claude
                                .request(
                                    "session/steer",
                                    json!({
                                        "sessionId": backing,
                                        "input": params.get("input").cloned().unwrap_or_else(|| json!([])),
                                        "expectedTurnId": params
                                            .get("expectedTurnId")
                                            .cloned()
                                            .unwrap_or(Value::Null),
                                        "handoffContext": turn_context
                                    }),
                                )
                                .await;
                        }
                        self.claude
                            .request(
                                "session/prompt",
                                json!({
                                    "sessionId": backing,
                                    "input": params.get("input").cloned().unwrap_or_else(|| json!([])),
                                    "model": params.get("model").cloned().unwrap_or(Value::Null),
                                    "effort": params.get("effort").cloned().unwrap_or(Value::Null),
                                    "permissionMode": CLAUDE_PREFERRED_PERMISSION_MODE,
                                    "handoffContext": turn_context
                                }),
                            )
                            .await
                    } else if selected == RuntimeKind::OpenCode {
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
                        let turn = self
                            .open_code()?
                            .start_prompt_content(
                                &backing,
                                input,
                                turn_context.as_deref(),
                                method == "turn/steer",
                            )
                            .await?;
                        Ok(json!({ "turn": { "id": turn } }))
                    } else {
                        let backing = self.ensure_codex_route(&visible, &params).await?;
                        prepare_codex_turn_context(&mut params);
                        params["threadId"] = json!(backing);
                        self.codex()?.request(method, params).await
                    }
                }
                .await;

                if response.is_ok() {
                    if selected == RuntimeKind::Claude {
                        self.note_claude_selection(
                            &visible,
                            turn_model.as_deref(),
                            turn_effort.as_deref(),
                        );
                    } else if selected == RuntimeKind::OpenCode {
                        self.note_open_code_selection(
                            &visible,
                            turn_model.as_deref(),
                            turn_effort.as_deref(),
                        );
                    }
                    if let Some(snapshot) = snapshot {
                        if switching {
                            self.note_seen_through(&visible, previous, snapshot.last_block_id);
                        }
                        self.note_seen_through(&visible, selected, snapshot.last_block_id);
                    }
                } else if switching {
                    self.restore_active_route(&visible, previous);
                }
                response
            }
            // Only Claude can answer this today. An unknown runtime reports nothing
            // so the host leaves its spinner alone rather than guessing.
            "turn/status" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) != RuntimeKind::Claude {
                    return Ok(json!({ "known": false }));
                }
                let backing = self.backing_id(visible, RuntimeKind::Claude)?;
                self.claude
                    .request("session/turnStatus", json!({ "sessionId": backing }))
                    .await
            }
            "turn/interrupt" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::Claude {
                    let backing = self.backing_id(visible, RuntimeKind::Claude)?;
                    self.claude
                        .request("session/interrupt", json!({ "sessionId": backing }))
                        .await
                } else if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let backing = self.backing_id(visible, RuntimeKind::OpenCode)?;
                    self.open_code()?.cancel(&backing)?;
                    Ok(json!({}))
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    self.codex()?.request(method, params).await
                }
            }
            "thread/fork" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::Claude {
                    let route = self.route(visible);
                    let backing = self.backing_id(visible, RuntimeKind::Claude)?;
                    let cwd = route
                        .as_ref()
                        .map(|route| route.cwd.clone())
                        .unwrap_or_else(|| self.cwd.clone());
                    let mut response = self
                        .claude
                        .request("session/fork", {
                            let mut request = claude_session_params(&params, &cwd, None);
                            request["sessionId"] = json!(backing);
                            request
                        })
                        .await?;
                    self.register_claude_response(&mut response, cwd)?;
                    // The fork's own route has no turns yet, so remember what it
                    // opened on; a later resume would otherwise fall back to the
                    // bridge defaults instead of the parent's selection.
                    if let Some(forked) = response
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                    {
                        let model = response
                            .get("model")
                            .and_then(Value::as_str)
                            .or_else(|| params.get("model").and_then(Value::as_str))
                            .map(ToOwned::to_owned);
                        let effort = response
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .filter(|effort| !effort.is_empty())
                            .or_else(|| params.get("effort").and_then(Value::as_str))
                            .map(ToOwned::to_owned);
                        self.note_claude_selection(&forked, model.as_deref(), effort.as_deref());
                    }
                    Ok(response)
                } else if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let route = self.route(visible);
                    let backing = route
                        .as_ref()
                        .and_then(|route| route.open_code_id.as_deref())
                        .unwrap_or(visible);
                    let cwd = route
                        .as_ref()
                        .map(|route| route.cwd.clone())
                        .unwrap_or_else(|| self.cwd.clone());
                    let model = params.get("model").and_then(Value::as_str);
                    let effort = params
                        .get("effort")
                        .and_then(Value::as_str)
                        .filter(|effort| !effort.is_empty());
                    let response = self
                        .open_code()?
                        .fork_session(&cwd, backing, model, effort)
                        .await?;
                    let id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .context("OpenCode thread/fork 응답에 id가 없습니다.")?;
                    self.register_route(
                        id,
                        RuntimeKind::OpenCode,
                        None,
                        Some(id.to_owned()),
                        None,
                        cwd,
                    );
                    self.note_open_code_selection(
                        id,
                        response.get("model").and_then(Value::as_str),
                        response.get("reasoningEffort").and_then(Value::as_str),
                    );
                    Ok(response)
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    let response = self.codex()?.request(method, params).await?;
                    self.register_codex_response(&response);
                    Ok(response)
                }
            }
            "thread/compact/start" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) == RuntimeKind::Claude {
                    let backing = self.backing_id(visible, RuntimeKind::Claude)?;
                    self.claude
                        .request("session/compact", json!({ "sessionId": backing }))
                        .await
                } else if self.route_kind(visible) == RuntimeKind::OpenCode {
                    let backing = self.backing_id(visible, RuntimeKind::OpenCode)?;
                    let turn = self.open_code()?.start_prompt(&backing, "/compact").await?;
                    Ok(json!({ "turn": { "id": turn } }))
                } else {
                    params["threadId"] = json!(self.backing_id(visible, RuntimeKind::Codex)?);
                    self.codex()?.request(method, params).await
                }
            }
            "claude/permissions/status" => self.claude.request("permissions/status", params).await,
            "claude/account/read" => self.claude.request("account/read", params).await,
            "claude/permissions/update" => self.claude.request("permissions/update", params).await,
            "claude/permissions/auto-mode" => {
                self.claude.request("permissions/auto-mode", params).await
            }
            "claude/permissions/retry" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) != RuntimeKind::Claude {
                    return Err(anyhow::anyhow!(
                        "Claude 세션에서만 권한 거부를 재시도할 수 있습니다."
                    ));
                }
                params["sessionId"] = json!(self.backing_id(visible, RuntimeKind::Claude)?);
                self.claude.request("permissions/retry", params).await
            }
            // Only Claude has permission modes. A thread on another runtime — or
            // one Claude has not started yet — keeps the mode the next turn carries.
            "thread/permissionMode/set" => {
                let visible = thread_id(&params)?;
                if self.route_kind(visible) != RuntimeKind::Claude {
                    return Ok(json!({}));
                }
                let Ok(backing) = self.backing_id(visible, RuntimeKind::Claude) else {
                    return Ok(json!({}));
                };
                self.claude
                    .request(
                        "session/permissionMode",
                        json!({
                            "sessionId": backing,
                            "permissionMode": CLAUDE_PREFERRED_PERMISSION_MODE
                        }),
                    )
                    .await
            }
            "thread/unsubscribe" if self.route_kind(thread_id(&params)?) == RuntimeKind::Claude => {
                let visible = thread_id(&params)?;
                let backing = self.backing_id(visible, RuntimeKind::Claude)?;
                self.claude
                    .request(
                        "session/close",
                        json!({ "sessionId": backing, "delete": true }),
                    )
                    .await
            }
            "thread/settings/update" | "thread/unsubscribe"
                if matches!(
                    self.route_kind(thread_id(&params)?),
                    RuntimeKind::OpenCode | RuntimeKind::Claude
                ) =>
            {
                Ok(json!({}))
            }
            "config/value/write" if vibe_setting_write(&params)? => Ok(json!({})),
            "config/value/write" if provider_default_write(&params)? => Ok(json!({})),
            _ => self.codex()?.request(method, params).await,
        }
    }

    /// Reads only Codex's model catalogue. The ordinary `model/list` route also
    /// merges Claude and OpenCode entries, which is useful at startup but would
    /// needlessly refresh other providers when `/new` only needs Codex changes.
    pub async fn codex_model_catalog(&self) -> Result<Value> {
        self.codex()?
            .request(
                "model/list",
                json!({ "includeHidden": false, "limit": 100 }),
            )
            .await
    }

    pub fn client(&self) -> Option<AppServerClient> {
        self.codex.as_ref().map(AppServer::client)
    }

    pub fn integration_client(&self, model: &str) -> Option<IntegrationClient> {
        if is_claude_model(model) {
            Some(IntegrationClient::Claude(self.claude.client()))
        } else if is_open_code_model(model) {
            None
        } else {
            self.client().map(IntegrationClient::Codex)
        }
    }

    pub fn open_code_provider_api(&self) -> Option<crate::open_code::ProviderAuthServer> {
        self.open_code.as_ref().map(OpenCodeServer::provider_api)
    }

    pub async fn integration_request(
        &self,
        model: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.integration_client(model)
            .context("현재 provider는 플러그인 관리를 지원하지 않습니다.")?
            .request(method, params)
            .await
    }

    pub fn codex_thread_id(&self, visible: &str) -> Option<String> {
        self.route(visible)
            .and_then(|route| route.codex_id)
            .or_else(|| {
                (!is_claude_thread(visible) && !visible.starts_with("ses_"))
                    .then(|| visible.to_owned())
            })
    }

    pub fn active_codex_thread_id(&self, visible: &str) -> Option<String> {
        (self.route_kind(visible) == RuntimeKind::Codex)
            .then(|| self.codex_thread_id(visible))
            .flatten()
    }

    /// Whether a thread would resume into Codex. A relaunch knows nothing about the
    /// session it is about to restore beyond its id, and the answer decides whether
    /// the Codex app-server has to be up before the model and session lists are read.
    pub fn thread_is_codex(&self, visible: &str) -> bool {
        self.route_kind(visible) == RuntimeKind::Codex
    }

    pub async fn prepare_resume_runtime(&mut self, visible: &str) -> Result<()> {
        let visible = self.canonical_visible(visible);
        match self.route_kind(&visible) {
            RuntimeKind::Codex => self.start_codex().await,
            RuntimeKind::OpenCode => self.ensure_open_code().await.map(|_| ()),
            RuntimeKind::Claude => Ok(()),
        }?;
        // A mixed session's visible history may need both native transcripts for
        // the first resume after this feature is installed. Claude is always
        // available through the bridge; starting Codex here makes the fallback
        // migration possible even when Claude is the active provider. Failure is
        // deliberately non-fatal: a newer session-history sidecar can restore the
        // visible transcript without the inactive runtime.
        if self.is_mixed_provider_route(&visible) && self.codex.is_none() {
            let _ = self.start_codex().await;
        }
        Ok(())
    }

    pub async fn provider_catalog(&mut self) -> Result<Value> {
        self.ensure_open_code().await?.provider_catalog().await
    }

    pub async fn set_provider_api_key(
        &mut self,
        provider_id: &str,
        key: &str,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        self.ensure_open_code()
            .await?
            .set_provider_api_key(provider_id, key, inputs)
            .await
    }

    pub async fn authorize_provider_oauth(
        &mut self,
        provider_id: &str,
        method: usize,
        inputs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value> {
        self.ensure_open_code()
            .await?
            .authorize_provider_oauth(provider_id, method, inputs)
            .await
    }

    pub async fn complete_provider_oauth(
        &mut self,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<()> {
        self.ensure_open_code()
            .await?
            .complete_provider_oauth(provider_id, method, code)
            .await
    }

    pub fn has_open_code(&self) -> bool {
        self.open_code.is_some()
    }

    pub async fn start_open_code(&mut self) -> Result<()> {
        self.ensure_open_code().await.map(|_| ())
    }

    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        if is_claude_request_id(&id) {
            self.claude.respond(id, result)
        } else if is_open_code_request_id(&id) {
            self.open_code()?.respond(id, result)
        } else {
            self.codex()?.respond(id, result)
        }
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        if is_claude_request_id(&id) {
            self.claude.respond_error(id, code, message)
        } else if is_open_code_request_id(&id) {
            self.open_code()?.respond_error(id, code, message)
        } else {
            self.codex()?.respond_error(id, code, message)
        }
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        if let Some(pending) = self
            .pending_events
            .lock()
            .expect("pending events mutex")
            .pop_front()
        {
            return Some(pending);
        }
        let (source, event) = match (self.codex.as_mut(), self.open_code.as_mut()) {
            (Some(codex), Some(open_code)) => tokio::select! {
                event = codex.next_event() => (RuntimeKind::Codex, event),
                event = open_code.next_event() => (RuntimeKind::OpenCode, event),
                event = self.claude.next_event() => (RuntimeKind::Claude, event),
            },
            (Some(codex), None) => tokio::select! {
                event = codex.next_event() => (RuntimeKind::Codex, event),
                event = self.claude.next_event() => (RuntimeKind::Claude, event),
            },
            (None, Some(open_code)) => tokio::select! {
                event = open_code.next_event() => (RuntimeKind::OpenCode, event),
                event = self.claude.next_event() => (RuntimeKind::Claude, event),
            },
            (None, None) => (RuntimeKind::Claude, self.claude.next_event().await),
        };
        if source == RuntimeKind::Codex
            && (event.is_none() || matches!(event, Some(ServerEvent::Closed(_))))
        {
            let detail = match event {
                Some(ServerEvent::Closed(detail)) => detail,
                _ => "Codex app-server 이벤트 채널이 종료되었습니다.".to_owned(),
            };
            self.codex_unavailable_reason = Some(detail.clone());
            if let Some(codex) = self.codex.take() {
                tokio::spawn(codex.shutdown());
            }
            return Some(ServerEvent::ProviderUnavailable {
                provider: "Codex".to_owned(),
                message: detail,
            });
        }
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
        if source == RuntimeKind::Claude
            && (event.is_none() || matches!(event, Some(ServerEvent::Closed(_))))
        {
            let detail = match event {
                Some(ServerEvent::Closed(detail)) => detail,
                _ => "Claude SDK 이벤트 채널이 종료되었습니다.".to_owned(),
            };
            return Some(ServerEvent::ProtocolWarning(detail));
        }
        event.map(|event| {
            let event = self.absorb_claude_rebind(event);
            self.rewrite_event(event)
        })
    }

    /// The Claude bridge renames a session when the CLI persists it under an id other
    /// than the one the bridge proposed. The route has to follow, or every later
    /// `session/history`, `session/resume`, and DevezCode `-r` would key off an id
    /// with no transcript behind it. Only the backing id moves here — whether that
    /// changes the id the thread resumes from is `note_resume_id`'s call, since a room
    /// whose turns have moved to Codex resumes from its rollout either way.
    fn absorb_claude_rebind(&self, event: ServerEvent) -> ServerEvent {
        let ServerEvent::Notification { method, params } = &event else {
            return event;
        };
        if method != "claude/session/rebound" {
            return event;
        }
        let Some(previous) = params.get("threadId").and_then(Value::as_str) else {
            return event;
        };
        let Some(next) = params.get("newThreadId").and_then(Value::as_str) else {
            return event;
        };
        let previous_backing = raw_thread_id(previous).to_owned();
        let next_backing = raw_thread_id(next).to_owned();
        if previous_backing == next_backing {
            return event;
        }
        let visible = self.visible_id(&previous_backing, previous);
        let route = self.route(&visible);
        let cwd = route
            .as_ref()
            .map(|route| route.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        self.register_route(
            &visible,
            route
                .as_ref()
                .map(|route| route.active)
                .unwrap_or(RuntimeKind::Claude),
            route.as_ref().and_then(|route| route.codex_id.clone()),
            route.as_ref().and_then(|route| route.open_code_id.clone()),
            Some(next_backing),
            cwd,
        );
        event
    }

    pub async fn shutdown(self) {
        let Self {
            codex,
            open_code,
            claude,
            ..
        } = self;
        tokio::join!(
            async move {
                if let Some(codex) = codex {
                    codex.shutdown().await;
                }
            },
            async move {
                if let Some(open_code) = open_code {
                    open_code.shutdown().await;
                }
            },
            claude.shutdown()
        );
    }

    fn open_code(&self) -> Result<&OpenCodeServer> {
        self.open_code
            .as_ref()
            .context("OpenCode가 설치되어 있지 않거나 ACP를 시작할 수 없습니다.")
    }

    fn codex(&self) -> Result<&AppServer> {
        self.codex
            .as_ref()
            .context("Codex app-server를 사용할 수 없습니다. Claude provider를 사용하세요.")
    }

    async fn ensure_open_code(&mut self) -> Result<&OpenCodeServer> {
        if !crate::open_code::PROVIDER_ENABLED {
            anyhow::bail!("OpenCode provider는 현재 비활성화되어 있습니다.");
        }
        if self.open_code.is_none() {
            self.open_code = Some(OpenCodeServer::spawn(&self.open_code_path, &self.cwd).await?);
            self.open_code
                .as_ref()
                .expect("OpenCode 서버를 방금 시작했습니다.")
                .initialize()
                .await?;
        }
        self.open_code()
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
        self.register_route(id, RuntimeKind::Codex, Some(id.to_owned()), None, None, cwd);
    }

    fn register_codex_response_as(&self, mut response: Value, visible: &str) -> Value {
        let Some(backing) = response
            .get("id")
            .or_else(|| response.pointer("/thread/id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            return response;
        };
        let cwd = response
            .get("cwd")
            .or_else(|| response.pointer("/thread/cwd"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.route(visible).map(|route| route.cwd))
            .unwrap_or_else(|| self.cwd.clone());
        response["id"] = json!(visible);
        if response.get("thread").is_some_and(Value::is_object) {
            response["thread"]["id"] = json!(visible);
        }
        let route = self.route(visible);
        self.register_route(
            visible,
            RuntimeKind::Codex,
            Some(backing),
            route.as_ref().and_then(|route| route.open_code_id.clone()),
            route.as_ref().and_then(|route| route.claude_id.clone()),
            cwd,
        );
        response
    }

    fn register_claude_response(&self, response: &mut Value, cwd: PathBuf) -> Result<()> {
        self.register_claude_response_as(response, cwd, None)
    }

    fn register_claude_response_as(
        &self,
        response: &mut Value,
        cwd: PathBuf,
        visible: Option<&str>,
    ) -> Result<()> {
        let backing = response
            .get("id")
            .or_else(|| response.pointer("/thread/id"))
            .and_then(Value::as_str)
            .context("Claude 세션 응답에 id가 없습니다.")?
            .to_owned();
        let visible = visible
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| visible_thread_id(&backing));
        response["id"] = json!(visible);
        if response.get("thread").is_some_and(Value::is_object) {
            response["thread"]["id"] = json!(visible);
        }
        let route = self.route(&visible);
        self.register_route(
            &visible,
            RuntimeKind::Claude,
            route.as_ref().and_then(|route| route.codex_id.clone()),
            route.as_ref().and_then(|route| route.open_code_id.clone()),
            Some(backing),
            cwd,
        );
        Ok(())
    }

    fn register_route(
        &self,
        visible: &str,
        active: RuntimeKind,
        codex_id: Option<String>,
        open_code_id: Option<String>,
        claude_id: Option<String>,
        cwd: PathBuf,
    ) {
        {
            let mut routes = self.routes.lock().expect("routes mutex");
            let route = routes.entry(visible.to_owned()).or_insert(Route {
                active,
                codex_id: None,
                open_code_id: None,
                claude_id: None,
                cwd: cwd.clone(),
                codex_seen_through: 0,
                open_code_seen_through: 0,
                claude_seen_through: 0,
                claude_model: None,
                claude_effort: None,
                open_code_model: None,
                open_code_effort: None,
                updated_at: 0,
            });
            route.active = active;
            route.cwd = cwd;
            if let Some(id) = codex_id {
                route.codex_id = Some(id);
            }
            if let Some(id) = open_code_id {
                route.open_code_id = Some(id);
            }
            if let Some(id) = claude_id {
                route.claude_id = Some(id);
            }
            route.touch();
            let aliases = route_aliases(&routes);
            *self.aliases.lock().expect("aliases mutex") = aliases;
        }
        self.persist_routes();
        self.note_resume_id(visible);
    }

    /// Raises a rebind notice when the id that resumes this thread stops being the
    /// thread's own id — a Claude-named room whose turns moved to Codex keeps its
    /// visible id, but the conversation now lives in a Codex rollout, and that is the
    /// id `/resume` and the DevezCode session file have to carry.
    fn note_resume_id(&self, visible: &str) {
        let Some(resume) = self.route(visible).as_ref().and_then(resume_id_for) else {
            return;
        };
        {
            let mut published = self.resume_ids.lock().expect("resume ids mutex");
            let previous = published
                .get(visible)
                .map(String::as_str)
                .unwrap_or(visible);
            if previous == resume {
                published.insert(visible.to_owned(), resume);
                return;
            }
            published.insert(visible.to_owned(), resume.clone());
        }
        self.pending_events
            .lock()
            .expect("pending events mutex")
            .push_back(ServerEvent::Notification {
                method: "thread/rebound".to_owned(),
                params: json!({ "threadId": visible, "newThreadId": resume }),
            });
    }

    /// The id a later launch has to pass to `-r` for this thread.
    pub fn resume_id(&self, visible: &str) -> String {
        self.route(visible)
            .as_ref()
            .and_then(resume_id_for)
            .unwrap_or_else(|| visible.to_owned())
    }

    /// Persists the portable transcript after a completed turn. Native Claude and
    /// Codex sessions remain the preferred history sources, while OpenCode and
    /// mixed sessions use this sidecar to restore their visible transcript.
    pub fn persist_provider_handoff(&self, visible: &str, snapshot: Value) {
        let Some(snapshot) = ProviderHandoff::from_value(snapshot) else {
            return;
        };
        let Some(path) = provider_history_path(visible) else {
            return;
        };
        let _ = save_provider_handoff(&path, &snapshot);
    }

    fn register_discovered_route(
        &self,
        visible: &str,
        kind: RuntimeKind,
        backing: &str,
        cwd: PathBuf,
    ) {
        let existing = self.route(visible);
        let active = existing.as_ref().map(|route| route.active).unwrap_or(kind);
        let mut codex_id = existing.as_ref().and_then(|route| route.codex_id.clone());
        let mut open_code_id = existing
            .as_ref()
            .and_then(|route| route.open_code_id.clone());
        let mut claude_id = existing.as_ref().and_then(|route| route.claude_id.clone());
        match kind {
            RuntimeKind::Codex => codex_id = Some(backing.to_owned()),
            RuntimeKind::OpenCode => open_code_id = Some(backing.to_owned()),
            RuntimeKind::Claude => claude_id = Some(backing.to_owned()),
        }
        self.register_route(visible, active, codex_id, open_code_id, claude_id, cwd);
    }

    fn visible_id(&self, backing: &str, fallback: &str) -> String {
        self.aliases
            .lock()
            .expect("aliases mutex")
            .get(backing)
            .cloned()
            .unwrap_or_else(|| fallback.to_owned())
    }

    fn persist_routes(&self) {
        let Some(path) = self.route_store_path.as_deref() else {
            return;
        };
        let routes = self.routes.lock().expect("routes mutex");
        let _ = save_routes(path, &routes);
    }

    fn route(&self, visible: &str) -> Option<Route> {
        self.routes
            .lock()
            .expect("routes mutex")
            .get(visible)
            .cloned()
    }

    fn is_mixed_provider_route(&self, visible: &str) -> bool {
        let visible = self.canonical_visible(visible);
        self.route(&visible)
            .is_some_and(|route| route.has_claude_codex_backings())
    }

    /// Reads both native histories for a legacy mixed route that has no visible
    /// transcript sidecar yet. Newer turns are persisted from the rendered
    /// transcript, while this path gives existing sessions a one-time migration
    /// instead of dropping the inactive provider's turns on resume.
    async fn mixed_provider_history(&self, visible: &str, params: &Value) -> Result<Value> {
        let route = self
            .route(visible)
            .context("혼합 provider 라우트를 찾을 수 없습니다.")?;
        let cwd = route.cwd.clone();
        let active = route.active;

        let claude_turns = match self
            .claude
            .request(
                "session/history",
                json!({
                    "sessionId": route
                        .claude_id
                        .as_deref()
                        .context("혼합 라우트에 Claude 세션 ID가 없습니다.")?,
                    "cwd": cwd
                }),
            )
            .await
        {
            Ok(response) => response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(error) if active == RuntimeKind::Claude => return Err(error),
            Err(_) => Vec::new(),
        };

        let codex_turns = match route.codex_id.as_deref() {
            Some(backing) if self.codex.is_some() => {
                match self.codex_history(params, backing).await {
                    Ok(turns) => turns,
                    Err(error) if active == RuntimeKind::Codex => return Err(error),
                    Err(_) => Vec::new(),
                }
            }
            _ if active == RuntimeKind::Codex => {
                return Err(anyhow::anyhow!(
                    "혼합 라우트의 Codex runtime을 시작하지 못했습니다."
                ));
            }
            _ => Vec::new(),
        };

        Ok(json!({
            "data": merge_provider_turns(
                claude_turns
                    .into_iter()
                    .chain(codex_turns)
                    .collect(),
            ),
            "nextCursor": null
        }))
    }

    async fn codex_history(&self, params: &Value, backing: &str) -> Result<Vec<Value>> {
        let mut turns = Vec::new();
        let mut cursor = None;
        let mut previous_cursor = None;

        loop {
            let mut request = params.clone();
            request["threadId"] = json!(backing);
            if let Some(cursor) = cursor.as_deref() {
                request["cursor"] = json!(cursor);
            } else if let Some(object) = request.as_object_mut() {
                object.remove("cursor");
            }
            let page = self.codex()?.request("thread/turns/list", request).await?;
            if let Some(data) = page.get("data").and_then(Value::as_array) {
                turns.extend(data.iter().cloned());
            }
            let next = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if next.is_none() || next == previous_cursor {
                break;
            }
            previous_cursor = cursor;
            cursor = next;
        }
        Ok(turns)
    }

    /// DevezCode stores the active native backing id for a mixed room. Resolve
    /// that id back to the visible route before any provider branch runs, so a
    /// restart does not create a new Codex-only route and hide the Claude half.
    fn canonical_visible(&self, id: &str) -> String {
        if self.route(id).is_some() {
            return id.to_owned();
        }
        let backing = raw_thread_id(id);
        self.aliases
            .lock()
            .expect("aliases mutex")
            .get(backing)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }

    fn route_kind(&self, visible: &str) -> RuntimeKind {
        let visible = self.canonical_visible(visible);
        self.route(&visible)
            .map(|route| route.active)
            .unwrap_or_else(|| id_runtime(&visible))
    }

    /// Remembers what a Claude turn ran on. Written on every turn and persisted
    /// with the route, so the next resume — this session or a later launch —
    /// reopens the thread on the model and effort it was actually using.
    fn note_claude_selection(&self, visible: &str, model: Option<&str>, effort: Option<&str>) {
        let model = model.filter(|model| is_claude_model(model));
        let effort = effort.filter(|effort| !effort.is_empty());
        if model.is_none() && effort.is_none() {
            return;
        }
        {
            let mut routes = self.routes.lock().expect("routes mutex");
            let Some(route) = routes.get_mut(visible) else {
                return;
            };
            if let Some(model) = model {
                route.claude_model = Some(model.to_owned());
            }
            if let Some(effort) = effort {
                route.claude_effort = Some(effort.to_owned());
            }
            route.touch();
        }
        self.persist_routes();
    }

    /// Remembers what an OpenCode turn ran on because ACP reloads the session on
    /// workspace defaults instead of reporting the session's previous selection.
    fn note_open_code_selection(&self, visible: &str, model: Option<&str>, effort: Option<&str>) {
        let model = model.filter(|model| is_open_code_model(model));
        let effort = effort.filter(|effort| !effort.is_empty());
        if model.is_none() && effort.is_none() {
            return;
        }
        {
            let mut routes = self.routes.lock().expect("routes mutex");
            let Some(route) = routes.get_mut(visible) else {
                return;
            };
            if let Some(model) = model {
                route.open_code_model = Some(model.to_owned());
            }
            if let Some(effort) = effort {
                route.open_code_effort = Some(effort.to_owned());
            }
            route.touch();
        }
        self.persist_routes();
    }

    fn note_seen_through(&self, visible: &str, kind: RuntimeKind, block_id: u64) {
        {
            if let Some(route) = self.routes.lock().expect("routes mutex").get_mut(visible) {
                route.note_seen_through(kind, block_id);
                route.touch();
            }
        }
        self.persist_routes();
    }

    fn restore_active_route(&self, visible: &str, active: RuntimeKind) {
        {
            if let Some(route) = self.routes.lock().expect("routes mutex").get_mut(visible) {
                route.active = active;
                route.touch();
            }
        }
        self.persist_routes();
    }

    fn is_open_code_thread(&self, visible: &str) -> bool {
        self.route_kind(visible) == RuntimeKind::OpenCode
    }

    fn backing_id(&self, visible: &str, kind: RuntimeKind) -> Result<String> {
        let visible = self.canonical_visible(visible);
        let route = self.route(&visible);
        let backing = match kind {
            RuntimeKind::Codex => route.and_then(|route| route.codex_id),
            RuntimeKind::OpenCode => route.and_then(|route| route.open_code_id),
            RuntimeKind::Claude => route.and_then(|route| route.claude_id),
        };
        backing
            .or_else(|| (self.route_kind(&visible) == kind).then(|| visible.to_owned()))
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
                route.claude_id,
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
        let response = self
            .open_code()?
            .start_session(&cwd, &model, params.get("effort").and_then(Value::as_str))
            .await?;
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
            self.route(visible).and_then(|route| route.claude_id),
            cwd,
        );
        Ok((backing, None))
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
                route.claude_id,
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
            .codex()?
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
        let claude_id = self.route(visible).and_then(|route| route.claude_id);
        self.register_route(
            visible,
            RuntimeKind::Codex,
            Some(backing.clone()),
            open_code_id,
            claude_id,
            cwd,
        );
        Ok(backing)
    }

    async fn ensure_claude_route(&self, visible: &str, params: &Value) -> Result<String> {
        if let Some(route) = self.route(visible)
            && let Some(backing) = route.claude_id
        {
            self.register_route(
                visible,
                RuntimeKind::Claude,
                route.codex_id,
                route.open_code_id,
                Some(backing.clone()),
                route.cwd,
            );
            return Ok(backing);
        }
        let cwd = self
            .route(visible)
            .map(|route| route.cwd)
            .unwrap_or_else(|| self.cwd.clone());
        let mut response = self
            .claude
            .request("session/start", claude_session_params(params, &cwd, None))
            .await?;
        let backing = response
            .get("id")
            .and_then(Value::as_str)
            .context("Claude 전환 세션에 id가 없습니다.")?
            .to_owned();
        let route = self.route(visible);
        self.register_route(
            visible,
            RuntimeKind::Claude,
            route.as_ref().and_then(|route| route.codex_id.clone()),
            route.as_ref().and_then(|route| route.open_code_id.clone()),
            Some(backing.clone()),
            cwd,
        );
        // The visible id intentionally remains the current UI thread when a model
        // switch creates this backing session. Only direct starts/resumes namespace it.
        response["id"] = json!(visible);
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

fn empty_list_response() -> Value {
    json!({ "data": [], "nextCursor": null })
}

fn store_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
}

fn route_store_path() -> Option<PathBuf> {
    if let Some(app_data) = env::var_os("APPDATA") {
        return Some(
            PathBuf::from(app_data)
                .join("DevezVibe")
                .join("session-routes.json"),
        );
    }
    env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join(".config")
            .join("devez-vibe")
            .join("session-routes.json")
    })
}

fn provider_history_path(visible: &str) -> Option<PathBuf> {
    let root = env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|app_data| app_data.join("DevezVibe").join("session-history"))
        .or_else(|| {
            env::var_os("HOME").map(PathBuf::from).map(|home| {
                home.join(".config")
                    .join("devez-vibe")
                    .join("session-history")
            })
        })?;
    let encoded = visible
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(root.join(format!("{encoded}.json")))
}

/// The runtime a thread id names on its own: Claude namespaces its sessions,
/// OpenCode prefixes `ses_`, and everything else is a Codex thread. Only used when
/// the route store has nothing on the id — a session that never mixed runtimes.
fn id_runtime(visible: &str) -> RuntimeKind {
    if is_claude_thread(visible) {
        RuntimeKind::Claude
    } else if visible.starts_with("ses_") {
        RuntimeKind::OpenCode
    } else {
        RuntimeKind::Codex
    }
}

fn load_routes(path: &Path) -> HashMap<String, Route> {
    fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn load_provider_handoff(visible: &str) -> Option<ProviderHandoff> {
    let path = provider_history_path(visible)?;
    let data = fs::read(path).ok()?;
    serde_json::from_slice::<Value>(&data)
        .ok()
        .and_then(ProviderHandoff::from_value)
}

fn save_routes(path: &Path, routes: &HashMap<String, Route>) -> Result<()> {
    // Every room DevezCode opens runs its own dvz, and each one only knows its own
    // threads. Writing just this process's map would drop the sibling rooms' routes,
    // and a dropped route is a room that resumes into the wrong runtime's session.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = acquire_store_lock(path)?;
    let mut stored = load_routes(path);
    for (visible, route) in routes.iter().filter(|(_, route)| route.is_worth_storing()) {
        let replace = stored.get(visible).is_none_or(|existing| {
            existing.updated_at == 0 || route.updated_at >= existing.updated_at
        });
        if replace {
            stored.insert(visible.clone(), route.clone());
        }
    }
    write_locked_json(path, &stored)
}

fn save_provider_handoff(path: &Path, snapshot: &ProviderHandoff) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = acquire_store_lock(path)?;
    write_locked_json(path, snapshot)
}

fn write_locked_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session-store.json");
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", process::id()));
    fs::write(&temp, bytes)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

struct StoreLock {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_store_lock(path: &Path) -> Result<StoreLock> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session-store.json");
    let lock_path = path.with_file_name(format!("{file_name}.lock"));
    for _ in 0..240 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => {
                return Ok(StoreLock {
                    path: lock_path,
                    _file: file,
                });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(600))
                {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!(
        "세션 저장소 잠금 획득 시간이 초과되었습니다: {}",
        path.display()
    )
}

fn route_aliases(routes: &HashMap<String, Route>) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    let mut ambiguous = HashSet::new();
    for (visible, route) in routes {
        for backing in [
            route.codex_id.as_ref(),
            route.open_code_id.as_ref(),
            route.claude_id.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            // A thread whose own id names this session does not compete for it — it
            // is that session, under the other spelling of the same id. Without this
            // an ordinary resume left two claims on one backing id, the alias was
            // dropped as ambiguous, and every notification the runtime sent for that
            // session became unroutable: the host discards what it cannot place, so
            // the turn's output and its completion both disappeared.
            let names_backing = thread_names_session(visible, backing);
            if ambiguous.contains(backing) && !names_backing {
                continue;
            }
            match aliases.get(backing) {
                Some(previous) if previous != visible => {
                    if names_backing {
                        aliases.insert(backing.clone(), visible.clone());
                        ambiguous.remove(backing);
                    } else if !thread_names_session(previous, backing) {
                        aliases.remove(backing);
                        ambiguous.insert(backing.clone());
                    }
                }
                None => {
                    aliases.insert(backing.clone(), visible.clone());
                }
                _ => {}
            }
        }
    }
    aliases
}

/// Whether this visible thread is the backing session itself rather than a thread
/// that merely runs on it. Claude ids appear both bare and `claude:`-prefixed.
fn thread_names_session(visible: &str, backing: &str) -> bool {
    visible == backing || visible == visible_thread_id(backing)
}

/// The id that resumes each stored thread — the backing session of the runtime the
/// thread was last active on, in the form `-r` accepts.
fn route_resume_ids(routes: &HashMap<String, Route>) -> HashMap<String, String> {
    routes
        .iter()
        .filter_map(|(visible, route)| resume_id_for(route).map(|resume| (visible.clone(), resume)))
        .collect()
}

fn resume_id_for(route: &Route) -> Option<String> {
    match route.active {
        RuntimeKind::Claude => route.claude_id.as_deref().map(visible_thread_id),
        RuntimeKind::Codex => route.codex_id.clone(),
        RuntimeKind::OpenCode => route.open_code_id.clone(),
    }
}

fn merge_provider_turns(turns: Vec<Value>) -> Vec<Value> {
    let mut indexed = turns.into_iter().enumerate().collect::<Vec<_>>();
    indexed.sort_by_key(|(index, turn)| (turn_timestamp(turn).unwrap_or(i64::MAX), *index));
    let mut seen = HashSet::new();
    indexed
        .into_iter()
        .filter_map(|(_, turn)| {
            let id = turn.get("id").and_then(Value::as_str);
            if let Some(id) = id
                && !seen.insert(id.to_owned())
            {
                return None;
            }
            Some(turn)
        })
        .collect()
}

fn turn_timestamp(turn: &Value) -> Option<i64> {
    for name in ["startedAt", "createdAt", "updatedAt"] {
        let Some(value) = turn.get(name) else {
            continue;
        };
        if let Some(timestamp) = value.as_i64() {
            return Some(timestamp);
        }
        if let Some(timestamp) = value.as_str()
            && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp)
        {
            return Some(parsed.timestamp());
        }
    }
    None
}

fn provider_handoff_history_page(snapshot: &ProviderHandoff) -> Value {
    let mut turns = Vec::new();
    let mut current_turn = None;

    for entry in &snapshot.entries {
        let item = provider_handoff_item(entry);
        let starts_turn = entry.kind == "user" || current_turn.is_none();
        if starts_turn {
            let turn_id = format!("devez-handoff-turn-{}", entry.id);
            turns.push(json!({
                "id": turn_id,
                "status": "completed",
                "model": (entry.kind == "user").then_some(entry.title.clone()),
                "durationMs": entry.response_duration_ms,
                "items": [item]
            }));
            current_turn = Some(turns.len() - 1);
        } else if let Some(index) = current_turn
            && let Some(items) = turns[index].get_mut("items").and_then(Value::as_array_mut)
        {
            items.push(item);
        }
    }

    json!({ "data": turns, "nextCursor": null })
}

fn provider_handoff_item(entry: &ProviderHandoffEntry) -> Value {
    let id = format!("devez-handoff-item-{}", entry.id);
    match entry.kind.as_str() {
        "user" => json!({
            "id": id,
            "type": "userMessage",
            "model": entry.title,
            "content": [{ "type": "text", "text": entry.body }]
        }),
        "assistant" => json!({
            "id": id,
            "type": "agentMessage",
            "provider": if entry.title.is_empty() { "Devez Vibe" } else { &entry.title },
            "text": entry.body
        }),
        "reasoning" => json!({
            "id": id,
            "type": "reasoning",
            "summary": [entry.body]
        }),
        "plan" => json!({
            "id": id,
            "type": "plan",
            "text": entry.body
        }),
        "tool" if entry.title.starts_with("Shell ·") => json!({
            "id": id,
            "type": "commandExecution",
            "command": entry.title.trim_start_matches("Shell ·").trim(),
            "status": "completed",
            "aggregatedOutput": entry.body
        }),
        "tool" => json!({
            "id": id,
            "type": "agentMessage",
            "provider": entry.title,
            "text": entry.body
        }),
        "file_change" => json!({
            "id": id,
            "type": "agentMessage",
            "provider": "File change",
            "text": entry.body
        }),
        _ => json!({
            "id": id,
            "type": "agentMessage",
            "provider": entry.title,
            "text": entry.body
        }),
    }
}

fn session_cwd(session: &Value, fallback: &Path) -> PathBuf {
    session
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn session_updated_at(session: &Value) -> u64 {
    session
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn merge_session(sessions: &mut Vec<Value>, session: Value) {
    let Some(id) = session.get("id").and_then(Value::as_str) else {
        sessions.push(session);
        return;
    };
    if let Some(existing) = sessions
        .iter_mut()
        .find(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
    {
        if session_updated_at(&session) >= session_updated_at(existing) {
            *existing = session;
        }
    } else {
        sessions.push(session);
    }
}

pub fn read_provider_config() -> String {
    provider_config_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default()
}

fn open_code_is_startup_default() -> bool {
    root_config_value(&read_provider_config(), "model").is_some_and(is_open_code_model)
}

fn provider_default_write(params: &Value) -> Result<bool> {
    let Some(key) = params.get("keyPath").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(value) = params.get("value").and_then(Value::as_str) else {
        return Ok(false);
    };
    if key == "model" {
        if is_open_code_model(value) || is_claude_model(value) {
            write_provider_config(value, "default")?;
            return Ok(true);
        }
        write_provider_config(value, "default")?;
        return Ok(false);
    }
    if key == "model_reasoning_effort" {
        let config = read_provider_config();
        if let Some(model) = root_config_value(&config, "model") {
            write_provider_config(model, value)?;
            return Ok(is_open_code_model(model) || is_claude_model(model));
        }
    }
    Ok(false)
}

fn vibe_setting_write(params: &Value) -> Result<bool> {
    let Some(key) = params.get("keyPath").and_then(Value::as_str) else {
        return Ok(false);
    };
    if !is_vibe_setting_key(key) {
        return Ok(false);
    }
    let value = params
        .get("value")
        .and_then(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .or_else(|| value.as_bool().map(|value| value.to_string()))
        })
        .context("Vibe 설정 값이 문자열 또는 boolean이 아닙니다.")?;
    crate::state::write_vibe_config_value(key, &value)?;
    Ok(true)
}

fn is_vibe_setting_key(key: &str) -> bool {
    matches!(
        key,
        "vibe_mode"
            | "conversation_view"
            | "model_verbosity"
            | "response_display_mode"
            | "shell_display_mode"
            | "diff_display_mode"
            | "side_panel_stage"
            | "status_line_model"
            | "status_line_effort"
            | "status_line_context"
            | "status_line_five_hour"
            | "status_line_weekly"
            | crate::state::CODEX_PROVIDER_KEY
            | crate::state::CLAUDE_PROVIDER_KEY
    )
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

const PROVIDER_HANDOFF_MAX_CHARS: usize = 120_000;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderHandoffEntry {
    id: u64,
    kind: String,
    title: String,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderHandoff {
    last_block_id: u64,
    cwd: String,
    plan: Option<String>,
    entries: Vec<ProviderHandoffEntry>,
}

impl ProviderHandoff {
    fn from_value(value: Value) -> Option<Self> {
        let last_block_id = value.get("lastBlockId")?.as_u64()?;
        let entries = value
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some(ProviderHandoffEntry {
                    id: entry.get("id")?.as_u64()?,
                    kind: entry.get("kind")?.as_str()?.to_owned(),
                    title: entry
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    body: entry
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    response_duration_ms: entry.get("responseDurationMs").and_then(Value::as_u64),
                })
            })
            .collect();
        Some(Self {
            last_block_id,
            cwd: value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            plan: value
                .get("plan")
                .and_then(Value::as_str)
                .filter(|plan| !plan.trim().is_empty())
                .map(ToOwned::to_owned),
            entries,
        })
    }

    fn context_since(
        &self,
        seen_through: u64,
        source: RuntimeKind,
        target: RuntimeKind,
    ) -> Option<String> {
        let unseen = self
            .entries
            .iter()
            .filter(|entry| entry.id > seen_through)
            .collect::<Vec<_>>();
        if unseen.is_empty() {
            return None;
        }
        let mut header = format!(
            "Devez Vibe가 {source}에서 {target}로 대화를 인계했습니다.\n\
             아래 내용은 같은 대화의 이전 기록입니다. 이미 완료된 작업을 반복하지 말고, \
             현재 사용자 요청에 바로 이어서 응답하세요. 인계 자체를 사용자에게 언급하지 마세요.\n\
             작업 경로: {}\n",
            self.cwd,
            source = source.label(),
            target = target.label(),
        );
        if let Some(plan) = self.plan.as_deref() {
            header.push_str("\n현재 작업 단계:\n");
            header.push_str(&tail_chars(plan, 20_000));
            header.push('\n');
        }
        header.push_str("\n이전 대화:\n");

        let sections = unseen
            .iter()
            .map(|entry| {
                let label = match entry.kind.as_str() {
                    "user" => "사용자",
                    "assistant" => "도우미",
                    "reasoning" => "작업 메모",
                    "plan" => "계획",
                    "tool" => "도구 실행",
                    "file_change" => "파일 변경",
                    _ => "기록",
                };
                let title = entry.title.trim();
                if title.is_empty() {
                    format!("[{label}]\n{}", entry.body.trim())
                } else {
                    format!("[{label} | {title}]\n{}", entry.body.trim())
                }
            })
            .collect::<Vec<_>>();
        let available = PROVIDER_HANDOFF_MAX_CHARS
            .saturating_sub(header.chars().count())
            .saturating_sub(160);
        let (sections, omitted) = newest_sections_within(&sections, available);
        if omitted > 0 {
            header.push_str(&format!(
                "[오래된 기록 {omitted}개는 대상 모델의 컨텍스트 보호를 위해 생략됨]\n\n"
            ));
        }
        header.push_str(&sections.join("\n\n"));
        Some(header)
    }
}

fn take_provider_handoff(params: &mut Value) -> Option<ProviderHandoff> {
    params
        .as_object_mut()?
        .remove("providerHandoff")
        .and_then(ProviderHandoff::from_value)
}

fn insert_handoff_context(params: &mut Value, context: &str) {
    if !params
        .get("additionalContext")
        .is_some_and(Value::is_object)
    {
        params["additionalContext"] = json!({});
    }
    params["additionalContext"]["provider-handoff"] = json!({
        "value": context,
        "kind": "application"
    });
}

/// Codex receives `additionalContext` directly. It already holds the rules as
/// the thread's developer instructions, so the standing copies are dropped here
/// along with the Claude-only entries, leaving the turn carrying the preset
/// alone.
fn prepare_codex_turn_context(params: &mut Value) {
    let Some(context) = params
        .get_mut("additionalContext")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    context.remove("devez-vibe-rules");
    context.remove("claude-devez-vibe-rules");
    context.remove("claude-devez-vibe-reminder");
}

fn combined_turn_instructions(params: &Value, runtime: RuntimeKind) -> Option<String> {
    // Claude holds the full rules as its system prompt and Codex as its thread
    // instructions, so neither turn restates them. OpenCode has no standing
    // instruction slot of its own, so unlike the other two it must carry the
    // shared rules with the turn or they never reach it.
    let rules = (runtime == RuntimeKind::OpenCode)
        .then(|| {
            params
                .pointer("/additionalContext/devez-vibe-rules/value")
                .and_then(Value::as_str)
        })
        .flatten();
    let mode = (runtime != RuntimeKind::OpenCode)
        .then(|| {
            params
                .pointer("/additionalContext/devez-vibe-mode/value")
                .and_then(Value::as_str)
        })
        .flatten();
    let claude_reminder = (runtime == RuntimeKind::Claude)
        .then(|| {
            params
                .pointer("/additionalContext/claude-devez-vibe-reminder/value")
                .and_then(Value::as_str)
        })
        .flatten();
    // The agent role is the one piece every runtime needs: OpenCode is skipped
    // for the mode notice, but a role that never reaches it would leave the
    // status line claiming a role the session is not in.
    let agent = params
        .pointer("/additionalContext/devez-vibe-agent/value")
        .and_then(Value::as_str);
    let parts = [
        rules,
        mode,
        params
            .pointer("/additionalContext/provider-handoff/value")
            .and_then(Value::as_str),
        agent,
        claude_reminder,
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn newest_sections_within(sections: &[String], budget: usize) -> (Vec<String>, usize) {
    if sections.is_empty() || budget == 0 {
        return (Vec::new(), sections.len());
    }
    let mut selected = Vec::new();
    let mut used = 0usize;
    for section in sections.iter().rev() {
        let separator = usize::from(!selected.is_empty()) * 2;
        let section_chars = section.chars().count();
        if used + separator + section_chars <= budget {
            selected.push(section.clone());
            used += separator + section_chars;
            continue;
        }
        if selected.is_empty() {
            selected.push(tail_chars(section, budget));
        }
        break;
    }
    selected.reverse();
    let omitted = sections.len().saturating_sub(selected.len());
    (selected, omitted)
}

fn tail_chars(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit {
        return value.to_owned();
    }
    value.chars().skip(count - limit).collect()
}

fn request_cwd(params: &Value) -> Option<PathBuf> {
    params.get("cwd").and_then(Value::as_str).map(PathBuf::from)
}

fn selected_runtime(model: Option<&str>, current: RuntimeKind) -> RuntimeKind {
    model.map_or(current, |model| {
        if is_claude_model(model) {
            RuntimeKind::Claude
        } else if is_open_code_model(model) {
            RuntimeKind::OpenCode
        } else {
            RuntimeKind::Codex
        }
    })
}

/// Reopens a resumed Claude session on what the thread's own turns ran on. An
/// explicit `--model`/`--effort` outranks the record; the host's saved default
/// stays behind in `claudeFallbackModel`/`claudeFallbackEffort`, where the bridge
/// reaches it only after the transcript's own model.
fn apply_remembered_claude_selection(params: &mut Value, route: Option<&Route>) {
    for (key, remembered) in [
        ("model", route.and_then(|route| route.claude_model.clone())),
        (
            "effort",
            route.and_then(|route| route.claude_effort.clone()),
        ),
    ] {
        let requested = params
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if requested {
            continue;
        }
        if let Some(value) = remembered {
            params[key] = json!(value);
        }
    }
}

fn apply_remembered_open_code_selection(params: &mut Value, route: Option<&Route>) {
    for (key, remembered) in [
        (
            "model",
            route.and_then(|route| route.open_code_model.clone()),
        ),
        (
            "effort",
            route.and_then(|route| route.open_code_effort.clone()),
        ),
    ] {
        let requested = params
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if requested {
            continue;
        }
        if let Some(value) = remembered {
            params[key] = json!(value);
        }
    }
}

fn claude_session_params(params: &Value, cwd: &Path, session_id: Option<&str>) -> Value {
    let mut request = json!({
        "cwd": cwd,
        "model": params.get("model").cloned().unwrap_or_else(|| json!("claude:default")),
        "systemPrompt": params
            .get("claudeDeveloperInstructions")
            .or_else(|| params.pointer("/additionalContext/claude-devez-vibe-rules/value"))
            .or_else(|| params.get("developerInstructions"))
            .cloned()
            .unwrap_or_else(|| json!(""))
    });
    if let Some(effort) = params
        .get("effort")
        .or_else(|| params.get("reasoningEffort"))
        .and_then(Value::as_str)
        .filter(|effort| !effort.is_empty())
    {
        request["effort"] = json!(effort);
    }
    request["permissionMode"] = json!(CLAUDE_PREFERRED_PERMISSION_MODE);
    // Last resort, below the transcript's own model: what the host would open a
    // new session on. The bridge applies these only when nothing better exists.
    for (from, to) in [
        ("claudeFallbackModel", "fallbackModel"),
        ("claudeFallbackEffort", "fallbackEffort"),
    ] {
        if let Some(value) = params
            .get(from)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            request[to] = json!(value);
        }
    }
    if let Some(session_id) = session_id {
        request["sessionId"] = json!(session_id);
    }
    request
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

    fn route(active: RuntimeKind, codex_id: Option<&str>, claude_id: Option<&str>) -> Route {
        Route {
            active,
            codex_id: codex_id.map(ToOwned::to_owned),
            open_code_id: None,
            claude_id: claude_id.map(ToOwned::to_owned),
            cwd: PathBuf::from("C:/repo"),
            codex_seen_through: 12,
            open_code_seen_through: 0,
            claude_seen_through: 7,
            claude_model: None,
            claude_effort: None,
            open_code_model: None,
            open_code_effort: None,
            updated_at: 0,
        }
    }

    /// The whole point of the record: a resumed thread reopens on what it ran on,
    /// not on the launch default the host offers as a fallback.
    #[test]
    fn a_resumed_claude_thread_prefers_what_its_own_turns_ran_on() {
        let mut remembered = route(RuntimeKind::Claude, None, Some("claude-uuid"));
        remembered.claude_model = Some("claude:opus".to_owned());
        remembered.claude_effort = Some("max".to_owned());
        let mut params = json!({
            "claudeFallbackModel": "claude:sonnet",
            "claudeFallbackEffort": "high"
        });

        apply_remembered_claude_selection(&mut params, Some(&remembered));

        assert_eq!(params["model"], json!("claude:opus"));
        assert_eq!(params["effort"], json!("max"));
    }

    /// A thread with no record leaves `model`/`effort` empty so the bridge can put
    /// the transcript's own model first; the saved default rides on as a fallback.
    #[test]
    fn a_claude_thread_with_no_record_leaves_the_choice_to_the_transcript() {
        let forgotten = route(RuntimeKind::Claude, None, Some("claude-uuid"));
        let mut params = json!({
            "claudeFallbackModel": "claude:sonnet",
            "claudeFallbackEffort": "high"
        });

        apply_remembered_claude_selection(&mut params, Some(&forgotten));

        assert!(params.get("model").is_none());
        assert!(params.get("effort").is_none());

        let request = claude_session_params(&params, Path::new("C:/repo"), Some("claude-uuid"));

        assert_eq!(request["fallbackModel"], json!("claude:sonnet"));
        assert_eq!(request["fallbackEffort"], json!("high"));
    }

    /// `--model`/`--effort` are the explicit ask, so neither the record nor the
    /// saved default may overwrite them.
    #[test]
    fn an_explicit_model_and_effort_outrank_the_remembered_selection() {
        let mut remembered = route(RuntimeKind::Claude, None, Some("claude-uuid"));
        remembered.claude_model = Some("claude:opus".to_owned());
        remembered.claude_effort = Some("max".to_owned());
        let mut params = json!({
            "model": "claude:fable",
            "effort": "low",
            "claudeFallbackModel": "claude:sonnet"
        });

        apply_remembered_claude_selection(&mut params, Some(&remembered));

        assert_eq!(params["model"], json!("claude:fable"));
        assert_eq!(params["effort"], json!("low"));
    }

    #[test]
    fn a_resumed_open_code_thread_prefers_what_its_own_turns_ran_on() {
        let mut remembered = route(RuntimeKind::OpenCode, None, None);
        remembered.open_code_id = Some("ses_1".to_owned());
        remembered.open_code_model = Some("opencode:anthropic/claude-sonnet".to_owned());
        remembered.open_code_effort = Some("high".to_owned());
        let mut params = json!({});

        apply_remembered_open_code_selection(&mut params, Some(&remembered));

        assert_eq!(params["model"], json!("opencode:anthropic/claude-sonnet"));
        assert_eq!(params["effort"], json!("high"));
        assert!(
            remembered.is_worth_storing(),
            "an OpenCode-only route must survive restart once it has a selection"
        );
    }

    #[test]
    fn an_explicit_open_code_selection_outranks_the_remembered_selection() {
        let mut remembered = route(RuntimeKind::OpenCode, None, None);
        remembered.open_code_model = Some("opencode:anthropic/claude-sonnet".to_owned());
        remembered.open_code_effort = Some("high".to_owned());
        let mut params = json!({
            "model": "opencode:openai/gpt-5",
            "effort": "low"
        });

        apply_remembered_open_code_selection(&mut params, Some(&remembered));

        assert_eq!(params["model"], json!("opencode:openai/gpt-5"));
        assert_eq!(params["effort"], json!("low"));
    }

    #[test]
    fn a_claude_named_thread_running_on_codex_resumes_from_its_rollout() {
        let switched = route(
            RuntimeKind::Codex,
            Some("019f-rollout"),
            Some("claude-uuid"),
        );
        assert_eq!(
            resume_id_for(&switched).as_deref(),
            Some("019f-rollout"),
            "the conversation lives in the rollout, so that is what -r has to name"
        );
        assert!(switched.is_worth_storing());

        let claude = route(RuntimeKind::Claude, None, Some("claude-uuid"));
        assert_eq!(
            resume_id_for(&claude).as_deref(),
            Some("claude:claude-uuid")
        );

        let codex = route(RuntimeKind::Codex, Some("019f-rollout"), None);
        assert_eq!(resume_id_for(&codex).as_deref(), Some("019f-rollout"));
        assert!(
            !codex.is_worth_storing(),
            "a codex-only thread is named after its own session"
        );
    }

    #[test]
    fn saving_routes_keeps_the_entries_other_rooms_wrote() {
        let dir = std::env::temp_dir().join("dvz-route-merge-test");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("session-routes.json");

        let mut first = HashMap::new();
        first.insert(
            "claude:room-one".to_owned(),
            route(RuntimeKind::Codex, Some("rollout-one"), Some("room-one")),
        );
        save_routes(&path, &first).expect("first room persists its route");

        let mut second = HashMap::new();
        second.insert(
            "claude:room-two".to_owned(),
            route(RuntimeKind::Codex, Some("rollout-two"), Some("room-two")),
        );
        save_routes(&path, &second).expect("second room persists its route");

        let stored = load_routes(&path);
        assert!(
            stored.contains_key("claude:room-one"),
            "a sibling room's dvz must not drop routes it never knew about"
        );
        assert!(stored.contains_key("claude:room-two"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_open_code_selection_survives_a_process_restart() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("dvz-opencode-route-{suffix}"));
        let path = dir.join("session-routes.json");
        let mut remembered = route(RuntimeKind::OpenCode, None, None);
        remembered.open_code_id = Some("ses_1".to_owned());
        remembered.open_code_model = Some("opencode:anthropic/claude-sonnet".to_owned());
        remembered.open_code_effort = Some("high".to_owned());

        save_routes(&path, &HashMap::from([("ses_1".to_owned(), remembered)])).unwrap();
        let restored = load_routes(&path);

        assert_eq!(
            restored["ses_1"].open_code_model.as_deref(),
            Some("opencode:anthropic/claude-sonnet")
        );
        assert_eq!(restored["ses_1"].open_code_effort.as_deref(), Some("high"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_handoff_history_rebuilds_both_sides_of_a_mixed_session() {
        let snapshot = ProviderHandoff::from_value(json!({
            "lastBlockId": 4,
            "cwd": "C:/repo",
            "entries": [
                { "id": 1, "kind": "user", "title": "claude:sonnet", "body": "Claude 질문", "responseDurationMs": 65000 },
                { "id": 2, "kind": "assistant", "title": "Claude", "body": "Claude 답변" },
                { "id": 3, "kind": "user", "title": "gpt-5", "body": "Codex 질문" },
                { "id": 4, "kind": "assistant", "title": "Codex", "body": "Codex 답변" }
            ]
        }))
        .expect("handoff snapshot");

        let page = provider_handoff_history_page(&snapshot);
        let turns = page["data"].as_array().expect("history turns");

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["durationMs"], json!(65_000));
        assert_eq!(turns[0]["items"][0]["type"], json!("userMessage"));
        assert_eq!(turns[0]["items"][1]["text"], json!("Claude 답변"));
        assert_eq!(
            turns[1]["items"][0]["content"][0]["text"],
            json!("Codex 질문")
        );
        assert_eq!(turns[1]["items"][1]["provider"], json!("Codex"));
    }

    #[test]
    fn duplicate_backing_ids_are_not_resolved_to_an_arbitrary_visible_session() {
        let routes = HashMap::from([
            (
                "room-one".to_owned(),
                route(RuntimeKind::Codex, Some("same-rollout"), Some("claude-one")),
            ),
            (
                "room-two".to_owned(),
                route(RuntimeKind::Codex, Some("same-rollout"), Some("claude-two")),
            ),
            (
                "room-three".to_owned(),
                route(
                    RuntimeKind::Claude,
                    Some("other-rollout"),
                    Some("claude-three"),
                ),
            ),
        ]);

        let aliases = route_aliases(&routes);

        assert!(!aliases.contains_key("same-rollout"));
        assert_eq!(
            aliases.get("other-rollout").map(String::as_str),
            Some("room-three")
        );
    }

    /// The session's own thread keeps the alias, so its notifications stay routable
    /// even after a resume left a second thread pointing at the same session.
    #[test]
    fn a_thread_named_after_its_session_outranks_one_that_only_runs_on_it() {
        let routes = HashMap::from([
            (
                "claude:session-one".to_owned(),
                route(RuntimeKind::Claude, None, Some("session-one")),
            ),
            (
                "019fe947-borrowed".to_owned(),
                route(RuntimeKind::Claude, None, Some("session-one")),
            ),
        ]);

        assert_eq!(
            route_aliases(&routes)
                .get("session-one")
                .map(String::as_str),
            Some("claude:session-one")
        );
    }

    #[test]
    fn a_stale_process_cannot_replace_a_newer_route_snapshot() {
        let dir = std::env::temp_dir().join("dvz-route-version-test");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("session-routes.json");

        let mut newer = route(RuntimeKind::Codex, Some("newer-rollout"), Some("claude"));
        newer.updated_at = 2;
        save_routes(&path, &HashMap::from([("room".to_owned(), newer)])).unwrap();

        let mut stale = route(RuntimeKind::Claude, Some("stale-rollout"), Some("claude"));
        stale.updated_at = 1;
        save_routes(&path, &HashMap::from([("room".to_owned(), stale)])).unwrap();

        assert_eq!(load_routes(&path)["room"].active, RuntimeKind::Codex);
        assert_eq!(
            load_routes(&path)["room"].codex_id.as_deref(),
            Some("newer-rollout")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_turns_are_merged_by_native_start_time() {
        let turns = merge_provider_turns(vec![
            json!({ "id": "codex", "startedAt": 20 }),
            json!({ "id": "claude", "startedAt": 10 }),
        ]);

        assert_eq!(turns[0]["id"], json!("claude"));
        assert_eq!(turns[1]["id"], json!("codex"));
    }

    #[tokio::test]
    async fn missing_codex_runtime_keeps_the_backend_available_for_claude() {
        let mut server = BackendServer::spawn(
            Path::new("devez-vibe-codex-does-not-exist-7f96e2"),
            Path::new("opencode"),
            Path::new("node"),
            Path::new("claude"),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .await
        .expect("Claude backend should survive a missing Codex executable");

        server.initialize().await.expect("fallback initialization");
        assert!(!server.has_codex());
        assert!(server.codex_unavailable_reason().is_none());
        assert!(server.start_codex().await.is_err());
        assert!(server.codex_unavailable_reason().is_some());
        server.shutdown().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_initialization_exit_keeps_the_backend_available_for_claude() {
        let mut server = BackendServer::spawn(
            Path::new("where.exe"),
            Path::new("opencode"),
            Path::new("node"),
            Path::new("claude"),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .await
        .expect("Claude backend should start without touching Codex");

        server.initialize().await.expect("fallback initialization");
        assert!(!server.has_codex());
        assert!(server.codex_unavailable_reason().is_none());
        assert!(server.start_codex().await.is_err());
        assert!(server.codex_unavailable_reason().is_some());
        server.shutdown().await;
    }

    #[test]
    fn explicit_model_switches_between_runtimes() {
        assert!(
            selected_runtime(Some("opencode:anthropic/claude"), RuntimeKind::Codex)
                == RuntimeKind::OpenCode
        );
        assert!(selected_runtime(Some("gpt-5.6-sol"), RuntimeKind::OpenCode) == RuntimeKind::Codex);
        assert!(selected_runtime(Some("claude:sonnet"), RuntimeKind::Codex) == RuntimeKind::Claude);
    }

    /// DevezCode relaunches a room with `dvz -r <thread>`, so an unrouted id is the
    /// only thing that says which backend the restored conversation belongs to.
    #[test]
    fn an_unrouted_thread_id_names_its_own_runtime() {
        assert!(id_runtime("019fcaac-0c2a-7cd3-97ae-f7513ba2f056") == RuntimeKind::Codex);
        assert!(id_runtime("claude:f1965f1e-8603-43b2-bfd5-628bacd21e5e") == RuntimeKind::Claude);
        assert!(id_runtime("ses_8fe37f877f8c") == RuntimeKind::OpenCode);
    }

    #[test]
    fn vibe_display_settings_are_local_but_provider_settings_are_not() {
        assert!(is_vibe_setting_key("vibe_mode"));
        assert!(is_vibe_setting_key("response_display_mode"));
        assert!(is_vibe_setting_key("shell_display_mode"));
        assert!(is_vibe_setting_key("diff_display_mode"));
        assert!(is_vibe_setting_key("side_panel_stage"));
        assert!(is_vibe_setting_key("status_line_context"));
        assert!(!is_vibe_setting_key("model"));
        assert!(!is_vibe_setting_key("plugins.example"));
    }

    #[test]
    fn claude_session_omits_an_unsupported_empty_effort() {
        let without_effort = claude_session_params(
            &json!({ "model": "claude:haiku", "effort": "" }),
            Path::new("C:/repo"),
            None,
        );
        assert!(without_effort.get("effort").is_none());

        let with_effort = claude_session_params(
            &json!({ "model": "claude:sonnet", "effort": "max" }),
            Path::new("C:/repo"),
            None,
        );
        assert_eq!(
            with_effort.get("effort").and_then(Value::as_str),
            Some("max")
        );
    }

    #[test]
    fn claude_sessions_always_request_bypass_permissions() {
        let request = claude_session_params(
            &json!({ "model": "claude:sonnet", "claudePermissionMode": "plan" }),
            Path::new("C:/repo"),
            None,
        );

        assert_eq!(
            request.get("permissionMode").and_then(Value::as_str),
            Some(CLAUDE_PREFERRED_PERMISSION_MODE)
        );
    }

    #[test]
    fn provider_handoff_only_carries_context_the_target_has_not_seen() {
        let snapshot = ProviderHandoff::from_value(json!({
            "lastBlockId": 12,
            "cwd": "C:/repo",
            "plan": "- [진행 중] 전환 검증",
            "entries": [
                { "id": 4, "kind": "user", "title": "Claude", "body": "이미 전달됨" },
                { "id": 9, "kind": "assistant", "title": "Claude", "body": "새 답변" },
                { "id": 12, "kind": "tool", "title": "Shell", "body": "cargo test" }
            ]
        }))
        .unwrap();

        let context = snapshot
            .context_since(4, RuntimeKind::Claude, RuntimeKind::Codex)
            .unwrap();

        assert!(!context.contains("이미 전달됨"));
        assert!(context.contains("새 답변"));
        assert!(context.contains("cargo test"));
        assert!(context.contains("Claude에서 Codex로"));
        assert!(context.contains("전환 검증"));
    }

    #[test]
    fn provider_handoff_is_private_to_the_router() {
        let mut params = json!({
            "threadId": "thread-1",
            "providerHandoff": {
                "lastBlockId": 1,
                "entries": []
            }
        });

        let snapshot = take_provider_handoff(&mut params).unwrap();

        assert_eq!(snapshot.last_block_id, 1);
        assert!(params.get("providerHandoff").is_none());
    }

    #[test]
    fn provider_handoff_keeps_the_newest_context_within_its_budget() {
        let old = "old".repeat(30_000);
        let latest = "latest".repeat(20_000);
        let snapshot = ProviderHandoff::from_value(json!({
            "lastBlockId": 2,
            "cwd": "C:/repo",
            "entries": [
                { "id": 1, "kind": "assistant", "body": old },
                { "id": 2, "kind": "assistant", "body": latest }
            ]
        }))
        .unwrap();

        let context = snapshot
            .context_since(0, RuntimeKind::Claude, RuntimeKind::Codex)
            .unwrap();

        assert!(context.contains("latestlatest"));
        assert!(!context.contains("oldold"));
        assert!(context.contains("오래된 기록 1개"));
        assert!(context.chars().count() <= PROVIDER_HANDOFF_MAX_CHARS);
    }

    #[test]
    fn handoff_context_joins_existing_turn_instructions() {
        let mut params = json!({
            "additionalContext": {
                "devez-vibe-rules": { "value": "codex rules", "kind": "application" },
                "claude-devez-vibe-rules": { "value": "claude rules", "kind": "application" },
                "devez-vibe-mode": { "value": "super vibe", "kind": "application" },
                "claude-devez-vibe-reminder": { "value": "claude reminder", "kind": "application" }
            }
        });
        insert_handoff_context(&mut params, "history");

        // Claude opens on the rules as its system prompt, so its turn carries
        // only the preset, handoff, and short output reminder. Codex holds the
        // rules as its thread instructions, so its turn carries the preset and
        // handoff. OpenCode has no standing instruction slot, so its turn has
        // to carry the shared rules itself, alongside the handoff.
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::Claude).as_deref(),
            Some("super vibe\n\nhistory\n\nclaude reminder")
        );
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::Codex).as_deref(),
            Some("super vibe\n\nhistory")
        );
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::OpenCode).as_deref(),
            Some("codex rules\n\nhistory")
        );
    }

    /// Every runtime needs the role, including the one that is skipped for the
    /// shared rules and the preset notice.
    #[test]
    fn the_agent_role_reaches_all_three_runtimes() {
        let params = json!({
            "additionalContext": {
                "devez-vibe-rules": { "value": "codex rules", "kind": "application" },
                "claude-devez-vibe-rules": { "value": "claude rules", "kind": "application" },
                "devez-vibe-mode": { "value": "super vibe", "kind": "application" },
                "claude-devez-vibe-reminder": { "value": "claude reminder", "kind": "application" },
                "devez-vibe-agent": { "value": "planner block", "kind": "application" }
            }
        });

        // The role sits after the handoff slot and before the output reminder.
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::Claude).as_deref(),
            Some("super vibe\n\nplanner block\n\nclaude reminder")
        );
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::Codex).as_deref(),
            Some("super vibe\n\nplanner block")
        );
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::OpenCode).as_deref(),
            Some("codex rules\n\nplanner block")
        );
    }

    /// A Standard turn with its reset already spent sends no role key, and the
    /// combined instructions have to look exactly as they did before roles.
    #[test]
    fn a_turn_without_a_role_key_is_unchanged() {
        let params = json!({
            "additionalContext": {
                "devez-vibe-mode": { "value": "super vibe", "kind": "application" }
            }
        });

        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::Claude).as_deref(),
            Some("super vibe")
        );
        assert_eq!(
            combined_turn_instructions(&params, RuntimeKind::OpenCode).as_deref(),
            None
        );
    }

    /// Codex strips the keys its thread instructions already hold; the role is
    /// not one of them.
    #[test]
    fn the_codex_boundary_keeps_the_agent_role() {
        let mut params = json!({
            "additionalContext": {
                "devez-vibe-rules": { "value": "full rules", "kind": "application" },
                "devez-vibe-agent": { "value": "planner block", "kind": "application" }
            }
        });

        prepare_codex_turn_context(&mut params);

        assert_eq!(
            params
                .pointer("/additionalContext/devez-vibe-agent/value")
                .and_then(Value::as_str),
            Some("planner block")
        );
    }

    #[test]
    fn codex_turn_keeps_only_the_preset_at_the_provider_boundary() {
        let mut params = json!({
            "additionalContext": {
                "devez-vibe-rules": { "value": "full rules", "kind": "application" },
                "claude-devez-vibe-rules": { "value": "claude full rules", "kind": "application" },
                "claude-devez-vibe-reminder": { "value": "claude reminder", "kind": "application" },
                "devez-vibe-mode": { "value": "mode", "kind": "application" }
            }
        });

        prepare_codex_turn_context(&mut params);

        // The thread's developer instructions already carry the rules; sending
        // them again with every turn only doubled them.
        assert!(
            params
                .pointer("/additionalContext/devez-vibe-rules")
                .is_none()
        );
        assert!(
            params
                .pointer("/additionalContext/claude-devez-vibe-rules")
                .is_none()
        );
        assert!(
            params
                .pointer("/additionalContext/claude-devez-vibe-reminder")
                .is_none()
        );
        assert_eq!(
            params
                .pointer("/additionalContext/devez-vibe-mode/value")
                .and_then(Value::as_str),
            Some("mode")
        );
    }

    #[test]
    fn mixed_provider_routes_survive_a_process_restart() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("devez-vibe-route-{suffix}"));
        let path = root.join("session-routes.json");
        let visible = "claude:11111111-1111-1111-1111-111111111111";
        let codex = "22222222-2222-2222-2222-222222222222";
        let claude = "11111111-1111-1111-1111-111111111111";
        let routes = HashMap::from([
            (
                visible.to_owned(),
                route(RuntimeKind::Codex, Some(codex), Some(claude)),
            ),
            (
                "33333333-3333-3333-3333-333333333333".to_owned(),
                route(
                    RuntimeKind::Codex,
                    Some("33333333-3333-3333-3333-333333333333"),
                    None,
                ),
            ),
        ]);

        save_routes(&path, &routes).unwrap();
        let restored = load_routes(&path);
        let aliases = route_aliases(&restored);

        assert_eq!(restored.len(), 1);
        assert!(matches!(restored[visible].active, RuntimeKind::Codex));
        assert_eq!(aliases.get(codex).map(String::as_str), Some(visible));
        assert_eq!(aliases.get(claude).map(String::as_str), Some(visible));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mixed_session_list_keeps_one_row_with_the_latest_preview() {
        let mut sessions = vec![json!({
            "id": "claude:session",
            "preview": "Claude 시작",
            "updatedAt": 10
        })];

        merge_session(
            &mut sessions,
            json!({
                "id": "claude:session",
                "preview": "Codex 후속 대화",
                "updatedAt": 20
            }),
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["preview"], "Codex 후속 대화");
    }
}
