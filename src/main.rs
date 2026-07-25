mod app_server;
mod editor;
mod renderer;
mod state;

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use app_server::{AppServer, ServerEvent};
use arboard::Clipboard;
use clap::Parser;
use crossterm::event::{Event, EventStream};
use editor::Editor;
use futures_util::StreamExt;
use renderer::{BlockKind, Renderer, TerminalSession, View};
use serde_json::{Value, json};
use state::{
    Action, AppState, ModelInfo, SessionInfo, SessionPicker, SessionPickerResult,
    load_model_context_windows,
};
use tokio::time::MissedTickBehavior;

#[derive(Parser)]
#[command(
    name = "devez",
    version,
    about = "Stable terminal UI for the official Codex app-server"
)]
struct Cli {
    /// Resume by ID/name, or open the session picker when no value is given.
    #[arg(
        short = 'r',
        long,
        value_name = "SESSION",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with = "continue_session"
    )]
    resume: Option<String>,

    /// Continue the most recent session in the current directory.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_session: bool,

    /// Select a model from the app-server model catalog.
    #[arg(long)]
    model: Option<String>,

    /// Select a supported reasoning effort (for example high, xhigh, max).
    #[arg(long)]
    effort: Option<String>,

    /// Working directory. New threads default to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Codex executable used to launch `codex app-server`.
    #[arg(long, default_value = "codex")]
    codex: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut server = AppServer::spawn(&cli.codex).await?;

    let result = run(&cli, &mut server).await;
    server.shutdown().await;
    result
}

async fn run(cli: &Cli, server: &mut AppServer) -> Result<()> {
    server.initialize().await?;
    let account = ensure_account(server).await?;

    let models_response = server
        .request(
            "model/list",
            json!({ "includeHidden": false, "limit": 100 }),
        )
        .await?;
    let models = parse_models(&models_response);
    if models.is_empty() {
        bail!("app-server가 사용 가능한 모델을 반환하지 않았습니다.");
    }

    let requested_model_name = choose_model(&models, cli.model.as_deref())?.model.clone();
    let cwd = resolve_cwd(cli.cwd.as_deref())?;
    let resume_id = resolve_startup_session(cli, server, &cwd).await?;
    let Some(resume_id) = resume_id else {
        return Ok(());
    };
    let is_resuming = !resume_id.is_empty();
    let model_override = if is_resuming {
        cli.model.as_deref()
    } else {
        Some(
            cli.model
                .as_deref()
                .unwrap_or(requested_model_name.as_str()),
        )
    };
    let thread_response = start_or_resume_thread(
        server,
        is_resuming.then_some(resume_id.as_str()),
        model_override,
        cli.cwd.as_ref().map(|_| cwd.as_path()),
        &cwd,
    )
    .await?;

    let thread = thread_response
        .get("thread")
        .context("thread 응답에 thread가 없습니다.")?;
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .context("thread 응답에 id가 없습니다.")?
        .to_owned();
    let actual_model = thread_response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&requested_model_name)
        .to_owned();
    let actual_effort = cli.effort.clone().or_else(|| {
        thread_response
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    validate_effort(&models, &actual_model, actual_effort.as_deref())?;
    let actual_cwd = thread_response
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| cwd.to_str().unwrap_or("."))
        .to_owned();

    let mut state = AppState::new(
        thread_id,
        actual_cwd,
        account,
        models,
        &actual_model,
        actual_effort.as_deref(),
    );
    if is_resuming {
        state.load_history(thread);
    }

    let terminal = TerminalSession::enter()?;
    let mut renderer = Renderer::new();
    renderer.clear_screen()?;
    let ui_result = event_loop(server, &mut state, &mut renderer).await;
    let _ = renderer.finish();
    drop(terminal);
    ui_result
}

async fn resolve_startup_session(
    cli: &Cli,
    server: &AppServer,
    cwd: &Path,
) -> Result<Option<String>> {
    if cli.continue_session {
        let sessions = list_sessions(server, Some(cwd), None, 1).await?;
        let session = sessions
            .first()
            .context("이 작업 폴더에서 계속할 세션을 찾지 못했습니다.")?;
        return Ok(Some(session.id.clone()));
    }

    match cli.resume.as_deref() {
        None => Ok(Some(String::new())),
        Some("") => {
            let sessions = list_sessions(server, None, None, 100).await?;
            choose_startup_session(sessions, cwd).await
        }
        Some(target) => Ok(Some(
            resolve_session_target(server, target, Some(cwd)).await?,
        )),
    }
}

async fn choose_startup_session(sessions: Vec<SessionInfo>, cwd: &Path) -> Result<Option<String>> {
    let terminal = TerminalSession::enter()?;
    let mut renderer = Renderer::new();
    renderer.clear_screen()?;
    let mut picker = SessionPicker::new(sessions, cwd.to_string_lossy().into_owned(), None);
    let editor = Editor::default();
    let mut events = EventStream::new();

    let result = loop {
        renderer.render(
            &[],
            View {
                live_blocks: Vec::new(),
                overlay: Some(picker.overlay_view()),
                editor: &editor,
                welcome: None,
                suggestions: Vec::new(),
                activity: None,
                footer: "Resume a Codex session".to_owned(),
                status_line: None,
                composer_notice: None,
                composer_mode: None,
            },
        )?;
        match events.next().await {
            Some(Ok(Event::Key(key))) => match picker.handle_key(key) {
                SessionPickerResult::None => {}
                SessionPickerResult::Cancel => break Ok(None),
                SessionPickerResult::Select(thread_id) => break Ok(Some(thread_id)),
            },
            Some(Ok(Event::Paste(text))) => picker.handle_paste(&text),
            Some(Ok(Event::Resize(_, _))) => {}
            Some(Ok(_)) => {}
            Some(Err(error)) => break Err(error.into()),
            None => break Ok(None),
        }
    };

    let _ = renderer.finish();
    drop(terminal);
    result
}

async fn event_loop(
    server: &mut AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut activity_tick = tokio::time::interval(Duration::from_millis(120));
    activity_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    draw(state, renderer)?;

    loop {
        let mut connection_closed = false;
        let action = tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => state.handle_key(key),
                    Some(Ok(Event::Paste(text))) => {
                        state.handle_paste(&text);
                        Action::None
                    }
                    Some(Ok(Event::Resize(_, _))) => Action::None,
                    Some(Ok(_)) => Action::None,
                    Some(Err(error)) => {
                        state.push_notice(BlockKind::Error, "터미널 입력 오류", error.to_string());
                        Action::Quit
                    }
                    None => Action::Quit,
                }
            }
            server_event = server.next_event() => {
                match server_event {
                    Some(ServerEvent::Notification { method, params }) => {
                        state.handle_notification(&method, &params);
                        Action::None
                    }
                    Some(ServerEvent::Request { id, method, params }) => {
                        state.begin_server_request(id, &method, &params)
                    }
                    Some(ServerEvent::ProtocolWarning(message)) => {
                        state.push_notice(BlockKind::Warning, "프로토콜 경고", message);
                        Action::None
                    }
                    Some(ServerEvent::Closed(message)) => {
                        state.push_notice(BlockKind::Error, "연결 종료", message);
                        connection_closed = true;
                        Action::None
                    }
                    None => {
                        connection_closed = true;
                        Action::None
                    }
                }
            }
            _ = activity_tick.tick() => {
                Action::Tick(state.tick())
            }
        };

        let redraw = !matches!(&action, Action::Tick(false));
        let should_quit = execute_action(server, state, renderer, action).await?;
        if redraw {
            draw(state, renderer)?;
        }
        if should_quit || connection_closed {
            break;
        }
    }
    Ok(())
}

async fn execute_action(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    action: Action,
) -> Result<bool> {
    match action {
        Action::None => {}
        Action::Tick(_) => {}
        Action::Submit(text) => start_turn(server, state, text).await,
        Action::Steer(text) => {
            let Some(turn_id) = state.turn_id.clone() else {
                state.set_request_failed("활성 turn ID가 없어 추가 입력을 보낼 수 없습니다.");
                return Ok(false);
            };
            let params = json!({
                "threadId": state.thread_id,
                "expectedTurnId": turn_id,
                "input": [{
                    "type": "text",
                    "text": text,
                    "text_elements": []
                }]
            });
            if let Err(error) = server.request("turn/steer", params).await {
                state.push_notice(BlockKind::Error, "추가 입력 실패", error.to_string());
            }
        }
        Action::Interrupt => {
            if let Some(turn_id) = state.turn_id.clone() {
                let params = json!({
                    "threadId": state.thread_id,
                    "turnId": turn_id
                });
                if let Err(error) = server.request("turn/interrupt", params).await {
                    state.push_notice(BlockKind::Error, "중단 실패", error.to_string());
                }
            }
        }
        Action::NewThread => {
            let response = server
                .request(
                    "thread/start",
                    json!({
                        "cwd": state.cwd,
                        "model": state.selected_model_name(),
                        "serviceTier": state.service_tier(),
                        "sessionStartSource": "clear",
                        "threadSource": "devez-cli"
                    }),
                )
                .await;
            match response {
                Ok(response) => {
                    let thread_id = response
                        .get("thread")
                        .and_then(|thread| thread.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let cwd = response
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let model = response
                        .get("model")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let effort = response
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    if let (Some(thread_id), Some(cwd), Some(model)) = (thread_id, cwd, model) {
                        renderer.clear_screen()?;
                        state.prepare_new_thread();
                        state.set_thread(thread_id, cwd, &model, effort.as_deref());
                    } else {
                        state.set_request_failed("thread/start 응답이 올바르지 않습니다.");
                    }
                }
                Err(error) => state.set_request_failed(error.to_string()),
            }
        }
        Action::OpenResume => match list_sessions(server, None, None, 100).await {
            Ok(sessions) => state.open_session_picker(sessions),
            Err(error) => state.push_notice(BlockKind::Error, "세션 목록 실패", error.to_string()),
        },
        Action::ResumeThread(target) => {
            let current_cwd = state.cwd.clone();
            let result = async {
                let thread_id =
                    resolve_session_target(server, &target, Some(Path::new(&current_cwd))).await?;
                resume_into_state(server, state, renderer, &thread_id).await
            }
            .await;
            if let Err(error) = result {
                state.push_notice(BlockKind::Error, "세션 재개 실패", error.to_string());
            }
        }
        Action::SetFast(enabled) => {
            let service_tier = if enabled {
                state
                    .selected_model()
                    .and_then(|model| model.fast_service_tier.as_deref())
                    .unwrap_or("priority")
                    .to_owned()
            } else {
                "default".to_owned()
            };
            let update = server
                .request(
                    "thread/settings/update",
                    json!({
                        "threadId": state.thread_id,
                        "serviceTier": service_tier
                    }),
                )
                .await;
            match update {
                Ok(_) => {
                    state.set_fast_mode(enabled);
                    if let Err(error) = server
                        .request(
                            "config/value/write",
                            json!({
                                "keyPath": "service_tier",
                                "value": service_tier,
                                "mergeStrategy": "upsert"
                            }),
                        )
                        .await
                    {
                        state.push_notice(
                            BlockKind::Warning,
                            "Fast 설정 저장 실패",
                            error.to_string(),
                        );
                    }
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "Fast 전환 실패", error.to_string())
                }
            }
        }
        Action::StartSide(prompt) => {
            let response = server
                .request(
                    "thread/fork",
                    json!({
                        "threadId": state.thread_id,
                        "model": state.selected_model_name(),
                        "serviceTier": state.service_tier(),
                        "ephemeral": true,
                        "threadSource": "devez-cli"
                    }),
                )
                .await;
            match response {
                Ok(response) => {
                    let thread_id = response
                        .get("thread")
                        .and_then(|thread| thread.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let cwd = response
                        .get("cwd")
                        .and_then(Value::as_str)
                        .unwrap_or(&state.cwd)
                        .to_owned();
                    let model = response
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| state.selected_model_name())
                        .to_owned();
                    let effort = response
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    if let Some(thread_id) = thread_id {
                        renderer.clear_screen()?;
                        state.enter_side_thread(thread_id, cwd, &model, effort.as_deref());
                        if let Some(prompt) = prompt {
                            state.begin_side_prompt(prompt.clone());
                            start_turn(server, state, prompt).await;
                        }
                    } else {
                        state.push_notice(
                            BlockKind::Error,
                            "Side conversation failed",
                            "thread/fork 응답에 thread ID가 없습니다.",
                        );
                    }
                }
                Err(error) => state.push_notice(
                    BlockKind::Error,
                    "Side conversation failed",
                    error.to_string(),
                ),
            }
        }
        Action::ReturnFromSide => {
            let child_thread = state.thread_id.clone();
            let parent_thread = state.side_parent_thread_id().map(ToOwned::to_owned);
            if let Some(parent_thread) = parent_thread {
                match resume_into_state(server, state, renderer, &parent_thread).await {
                    Ok(()) => {
                        if let Err(error) = server
                            .request("thread/unsubscribe", json!({ "threadId": child_thread }))
                            .await
                        {
                            state.push_notice(
                                BlockKind::Warning,
                                "Side cleanup failed",
                                error.to_string(),
                            );
                        }
                    }
                    Err(error) => state.push_notice(
                        BlockKind::Error,
                        "메인 대화 복귀 실패",
                        error.to_string(),
                    ),
                }
            }
        }
        Action::Compact => {
            match server
                .request(
                    "thread/compact/start",
                    json!({ "threadId": state.thread_id }),
                )
                .await
            {
                Ok(_) => state.push_notice(
                    BlockKind::System,
                    "Compacting context",
                    "Codex가 대화 컨텍스트를 압축하고 있습니다.",
                ),
                Err(error) => state.push_notice(BlockKind::Error, "압축 실패", error.to_string()),
            }
        }
        Action::Copy(text) => {
            match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(&text)) {
                Ok(()) => state.set_copy_notice(text.chars().count()),
                Err(error) => state.push_notice(BlockKind::Error, "복사 실패", error.to_string()),
            }
        }
        Action::ShowDiff => {
            let output = tokio::process::Command::new("git")
                .args(["diff", "--no-ext-diff", "--"])
                .current_dir(&state.cwd)
                .output()
                .await;
            match output {
                Ok(output) if output.status.success() => {
                    let diff = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    state.push_notice(
                        BlockKind::System,
                        "Git diff",
                        if diff.is_empty() {
                            "No tracked changes".to_owned()
                        } else {
                            diff
                        },
                    );
                }
                Ok(output) => state.push_notice(
                    BlockKind::Error,
                    "Git diff 실패",
                    String::from_utf8_lossy(&output.stderr).trim(),
                ),
                Err(error) => {
                    state.push_notice(BlockKind::Error, "Git diff 실패", error.to_string())
                }
            }
        }
        Action::RpcResponse { id, result } => {
            if let Err(error) = server.respond(id, result) {
                state.push_notice(BlockKind::Error, "응답 전송 실패", error.to_string());
            }
        }
        Action::RpcError { id, message } => {
            if let Err(error) = server.respond_error(id, -32601, &message) {
                state.push_notice(BlockKind::Error, "오류 응답 실패", error.to_string());
            }
        }
        Action::ClearScreen => renderer.clear_screen()?,
        Action::Quit => return Ok(true),
    }
    Ok(false)
}

async fn start_turn(server: &AppServer, state: &mut AppState, text: String) {
    let params = json!({
        "threadId": state.thread_id,
        "input": [{
            "type": "text",
            "text": text,
            "text_elements": []
        }],
        "model": state.selected_model_name(),
        "effort": state.selected_effort(),
        "serviceTier": state.service_tier(),
        "permissions": state.permission_profile()
    });
    match server.request("turn/start", params).await {
        Ok(response) => {
            if let Some(turn_id) = response
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
            {
                state.set_turn_started(turn_id.to_owned());
            }
        }
        Err(error) => state.set_request_failed(error.to_string()),
    }
}

fn draw(state: &mut AppState, renderer: &mut Renderer) -> Result<()> {
    let committed = state.drain_committed();
    let view = state.view();
    renderer.render(&committed, view)
}

async fn ensure_account(server: &AppServer) -> Result<String> {
    let response = server
        .request("account/read", json!({ "refreshToken": false }))
        .await?;
    let requires_auth = response
        .get("requiresOpenaiAuth")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if requires_auth && response.get("account").is_none_or(Value::is_null) {
        bail!("OpenAI 로그인이 필요합니다. 공식 `codex login`을 먼저 실행하세요.");
    }
    let account = response.get("account").unwrap_or(&Value::Null);
    let label = match account.get("type").and_then(Value::as_str) {
        Some("chatgpt") => {
            let plan = account
                .get("planType")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("ChatGPT · {plan}")
        }
        Some("apiKey") => "OpenAI API key".to_owned(),
        Some("amazonBedrock") => "Amazon Bedrock".to_owned(),
        Some(other) => other.to_owned(),
        None => "Local provider".to_owned(),
    };
    Ok(label)
}

async fn start_or_resume_thread(
    server: &AppServer,
    resume: Option<&str>,
    model: Option<&str>,
    resume_cwd: Option<&Path>,
    new_cwd: &Path,
) -> Result<Value> {
    if let Some(thread_id) = resume {
        let mut params = json!({ "threadId": thread_id });
        if let Some(model) = model {
            params["model"] = json!(model);
        }
        if let Some(cwd) = resume_cwd {
            params["cwd"] = json!(cwd.to_string_lossy());
        }
        server.request("thread/resume", params).await
    } else {
        server
            .request(
                "thread/start",
                json!({
                    "cwd": new_cwd.to_string_lossy(),
                    "model": model,
                    "sessionStartSource": "startup",
                    "threadSource": "devez-cli"
                }),
            )
            .await
    }
}

async fn list_sessions(
    server: &AppServer,
    cwd: Option<&Path>,
    search: Option<&str>,
    limit: u64,
) -> Result<Vec<SessionInfo>> {
    let mut params = json!({
        "limit": limit,
        "sortKey": "updated_at",
        "sortDirection": "desc"
    });
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd.to_string_lossy());
    }
    if let Some(search) = search {
        params["searchTerm"] = json!(search);
    }
    let response = server.request("thread/list", params).await?;
    Ok(response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(SessionInfo::from_value)
        .collect())
}

async fn resolve_session_target(
    server: &AppServer,
    target: &str,
    cwd: Option<&Path>,
) -> Result<String> {
    if looks_like_thread_id(target) {
        return Ok(target.to_owned());
    }

    let sessions = list_sessions(server, None, Some(target), 100).await?;
    let exact = sessions.iter().find(|session| {
        session
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(target))
            && cwd.is_none_or(|cwd| path_matches(&session.cwd, cwd))
    });
    let fallback = sessions.iter().find(|session| {
        session
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(target))
    });
    exact
        .or(fallback)
        .or_else(|| (sessions.len() == 1).then(|| &sessions[0]))
        .map(|session| session.id.clone())
        .with_context(|| format!("`{target}` 세션을 찾을 수 없습니다."))
}

async fn resume_into_state(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread_id: &str,
) -> Result<()> {
    let response = server
        .request("thread/resume", json!({ "threadId": thread_id }))
        .await?;
    let thread = response
        .get("thread")
        .context("thread/resume 응답에 thread가 없습니다.")?
        .clone();
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .context("재개한 thread에 id가 없습니다.")?
        .to_owned();
    let cwd = response
        .get("cwd")
        .and_then(Value::as_str)
        .context("thread/resume 응답에 cwd가 없습니다.")?
        .to_owned();
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .context("thread/resume 응답에 model이 없습니다.")?
        .to_owned();
    let effort = response
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    renderer.clear_screen()?;
    state.prepare_resume();
    state.set_thread(id, cwd, &model, effort.as_deref());
    state.load_history(&thread);
    Ok(())
}

fn looks_like_thread_id(value: &str) -> bool {
    value.len() >= 32 && value.chars().filter(|ch| *ch == '-').count() >= 4
}

fn path_matches(value: &str, path: &Path) -> bool {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        value.eq_ignore_ascii_case(&path)
    }
    #[cfg(not(windows))]
    {
        value == path
    }
}

fn parse_models(response: &Value) -> Vec<ModelInfo> {
    let mut models = response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(ModelInfo::from_value)
        .collect::<Vec<_>>();
    load_model_context_windows(&mut models);
    models
}

fn choose_model<'a>(models: &'a [ModelInfo], requested: Option<&str>) -> Result<&'a ModelInfo> {
    if let Some(requested) = requested {
        return models
            .iter()
            .find(|model| model.matches_query(requested))
            .with_context(|| format!("모델 카탈로그에 `{requested}`가 없습니다."));
    }
    models
        .iter()
        .find(|model| model.is_default)
        .or_else(|| models.first())
        .context("기본 모델을 찾을 수 없습니다.")
}

fn validate_effort(models: &[ModelInfo], model_name: &str, effort: Option<&str>) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };
    let Some(model) = models
        .iter()
        .find(|model| model.id == model_name || model.model == model_name)
    else {
        return Ok(());
    };
    if !model.supports_effort(effort) {
        let supported = model
            .efforts
            .iter()
            .map(|effort| effort.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "`{}` 모델은 `{effort}` reasoning을 지원하지 않습니다. 지원값: {supported}",
            model.display_name
        );
    }
    Ok(())
}

fn resolve_cwd(requested: Option<&Path>) -> Result<PathBuf> {
    let path = requested
        .map(Path::to_path_buf)
        .unwrap_or(env::current_dir().context("현재 작업 폴더를 확인할 수 없습니다.")?);
    path.canonicalize()
        .with_context(|| format!("작업 폴더를 열 수 없습니다: {}", path.display()))
}
