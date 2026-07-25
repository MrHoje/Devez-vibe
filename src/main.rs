mod app_server;
mod editor;
mod renderer;
mod state;
mod theme;
mod update;

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
    AccountPlan, Action, AppState, ModelInfo, SessionInfo, SessionPicker, SessionPickerResult,
    load_model_context_windows,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};

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

    /// UI theme: minimal, soft, or dark.
    #[arg(long, value_name = "THEME")]
    theme: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let selected_theme = theme::load(cli.theme.as_deref())?;
    theme::set_current(selected_theme);
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
    state.set_account_plan(read_account_plan(server).await);
    let _ = refresh_integrations(server, &mut state, false).await;
    if is_resuming {
        state.load_history(thread);
    }

    let (update_tx, update_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(latest) = update::check_for_update().await {
            let _ = update_tx.send(latest).await;
        }
    });

    let terminal = TerminalSession::enter()?;
    let mut renderer = Renderer::new(theme::current());
    renderer.clear_screen()?;
    let ui_result = event_loop(server, &mut state, &mut renderer, update_rx).await;
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
    let mut renderer = Renderer::new(theme::current());
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
    update_rx: mpsc::Receiver<String>,
) -> Result<()> {
    let mut update_rx = Some(update_rx);
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
                        if state.take_account_refresh() {
                            refresh_account(server, state).await;
                        }
                        if method == "skills/changed" {
                            Action::RefreshSkills
                        } else {
                            Action::None
                        }
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
            Some(latest) = recv_update(&mut update_rx) => {
                state.push_update_available(&latest);
                Action::None
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

/// Waits for the background update check; parks forever once the channel is done.
async fn recv_update(receiver: &mut Option<mpsc::Receiver<String>>) -> Option<String> {
    let Some(channel) = receiver.as_mut() else {
        return std::future::pending().await;
    };
    let latest = channel.recv().await;
    if latest.is_none() {
        *receiver = None;
    }
    latest
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
            let input = state.turn_input(text);
            let params = json!({
                "threadId": state.thread_id,
                "expectedTurnId": turn_id,
                "input": input
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
                        let _ = refresh_integrations(server, state, true).await;
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
                        BlockKind::Diff,
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
        Action::ShowMcp => {
            match server
                .request(
                    "mcpServerStatus/list",
                    json!({
                        "threadId": state.thread_id,
                        "detail": "toolsAndAuthOnly",
                        "limit": 100
                    }),
                )
                .await
            {
                Ok(response) => state.push_notice(
                    BlockKind::System,
                    "MCP servers",
                    format_mcp_servers(&response),
                ),
                Err(error) => {
                    state.push_notice(BlockKind::Error, "MCP 목록 실패", error.to_string())
                }
            }
        }
        Action::McpLogin(name) => {
            match server
                .request(
                    "mcpServer/oauth/login",
                    json!({
                        "name": name,
                        "threadId": state.thread_id,
                        "timeoutSecs": 300
                    }),
                )
                .await
            {
                Ok(response) => {
                    if let Some(url) = response.get("authorizationUrl").and_then(Value::as_str) {
                        state.push_notice(
                            BlockKind::System,
                            "MCP login",
                            format!("브라우저에서 인증을 완료하세요.\n{url}"),
                        );
                        if let Err(error) = open_url(url) {
                            state.push_notice(
                                BlockKind::Warning,
                                "브라우저 열기 실패",
                                error.to_string(),
                            );
                        }
                    } else {
                        state.push_notice(
                            BlockKind::Error,
                            "MCP login 실패",
                            "authorizationUrl이 없습니다.",
                        );
                    }
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "MCP login 실패", error.to_string())
                }
            }
        }
        Action::StartLogin => {
            if let Some(login_id) = state.active_login_id() {
                state.push_notice(
                    BlockKind::Warning,
                    "이미 로그인 중",
                    format!("진행 중인 로그인이 있습니다: {login_id}"),
                );
                return Ok(false);
            }
            match server
                .request("account/login/start", json!({ "type": "chatgpt" }))
                .await
            {
                Ok(response) => {
                    let login_id = response.get("loginId").and_then(Value::as_str);
                    let auth_url = response.get("authUrl").and_then(Value::as_str);
                    match (login_id, auth_url) {
                        (Some(login_id), Some(auth_url)) => {
                            state.begin_login(login_id.to_owned(), auth_url.to_owned());
                            if let Err(error) = open_url(auth_url) {
                                state.push_notice(
                                    BlockKind::Warning,
                                    "브라우저 열기 실패",
                                    format!("{error}\n위 URL을 직접 열어주세요."),
                                );
                            }
                        }
                        _ => state.push_notice(
                            BlockKind::Error,
                            "로그인 실패",
                            "app-server가 loginId 또는 authUrl을 반환하지 않았습니다.",
                        ),
                    }
                }
                Err(error) => state.push_notice(BlockKind::Error, "로그인 실패", error.to_string()),
            }
        }
        Action::CancelLogin(login_id) => {
            // Drop the modal first; the server call is best-effort cleanup.
            state.cancel_login_notice();
            if let Err(error) = server
                .request("account/login/cancel", json!({ "loginId": login_id }))
                .await
            {
                state.push_notice(BlockKind::Warning, "로그인 취소 실패", error.to_string());
            }
        }
        Action::Logout => match server.request("account/logout", json!({})).await {
            Ok(_) => state.apply_logout(),
            Err(error) => state.push_notice(BlockKind::Error, "로그아웃 실패", error.to_string()),
        },
        Action::ShowPlugins => match list_plugins(server, &state.cwd).await {
            Ok(response) => {
                state.update_plugins(&response);
                state.push_notice(BlockKind::System, "Plugins", format_plugins(&response));
            }
            Err(error) => {
                state.push_notice(BlockKind::Error, "플러그인 목록 실패", error.to_string())
            }
        },
        Action::PreparePluginInstall(query) => match list_plugins(server, &state.cwd).await {
            Ok(response) => match resolve_plugin(&response, &query) {
                Some(plugin) if plugin.installed && !plugin.enabled => state.push_notice(
                    BlockKind::System,
                    "Already installed",
                    format!(
                        "{} · disabled\n/plugins enable {query}",
                        plugin.display_name
                    ),
                ),
                Some(plugin) if plugin.installed => {
                    state.push_notice(BlockKind::System, "Already installed", plugin.display_name)
                }
                Some(plugin) if !plugin.available => state.push_notice(
                    BlockKind::Error,
                    "설치할 수 없는 플러그인",
                    format!(
                        "{}은(는) 관리자 정책으로 비활성화되어 있습니다.",
                        plugin.display_name
                    ),
                ),
                Some(plugin) => {
                    let disclosure = plugin_install_disclosure(&plugin);
                    let marketplace = plugin.marketplace_name.clone();
                    let description = plugin.description.clone();
                    state.confirm_plugin_install(
                        state::PluginInstallTarget {
                            plugin_name: plugin.name,
                            marketplace_path: plugin.marketplace_path,
                            remote_marketplace_name: plugin.remote_marketplace_name,
                        },
                        &marketplace,
                        description.as_deref(),
                        disclosure,
                    );
                }
                None => state.push_notice(
                    BlockKind::Error,
                    "플러그인을 찾을 수 없음",
                    format!("{query}\n/plugins에서 정확한 이름을 확인하세요."),
                ),
            },
            Err(error) => {
                state.push_notice(BlockKind::Error, "플러그인 조회 실패", error.to_string())
            }
        },
        Action::PreparePluginUninstall(query) => match list_plugins(server, &state.cwd).await {
            Ok(response) => match resolve_plugin(&response, &query) {
                Some(plugin) if plugin.installed && !plugin.uninstall_allowed => {
                    state.push_notice(
                        BlockKind::Warning,
                        "제거할 수 없는 플러그인",
                        format!(
                            "{}은(는) 관리자에 의해 설치되었습니다.",
                            plugin.display_name
                        ),
                    );
                }
                Some(plugin) if plugin.installed => {
                    state.confirm_plugin_uninstall(state::PluginUninstallTarget {
                        plugin_id: plugin.id,
                        display_name: plugin.display_name,
                    });
                }
                Some(plugin) => state.push_notice(
                    BlockKind::Warning,
                    "설치되지 않은 플러그인",
                    plugin.display_name,
                ),
                None => state.push_notice(
                    BlockKind::Error,
                    "플러그인을 찾을 수 없음",
                    format!("{query}\n/plugins에서 정확한 이름을 확인하세요."),
                ),
            },
            Err(error) => {
                state.push_notice(BlockKind::Error, "플러그인 조회 실패", error.to_string())
            }
        },
        Action::SetPlugin { query, enabled } => match list_plugins(server, &state.cwd).await {
            Ok(response) => match resolve_plugin(&response, &query) {
                Some(plugin) if !plugin.installed => state.push_notice(
                    BlockKind::Warning,
                    "설치되지 않은 플러그인",
                    format!("{}\n먼저 /plugins install {query}", plugin.display_name),
                ),
                Some(plugin) if !plugin.toggle_allowed => state.push_notice(
                    BlockKind::Warning,
                    "변경할 수 없는 플러그인",
                    format!("{}은(는) 관리자 정책으로 관리됩니다.", plugin.display_name),
                ),
                Some(plugin) if plugin.enabled == enabled => state.push_notice(
                    BlockKind::System,
                    "Plugin unchanged",
                    format!(
                        "{} · already {}",
                        plugin.display_name,
                        if enabled { "enabled" } else { "disabled" }
                    ),
                ),
                Some(plugin) => {
                    let result = server
                        .request(
                            "config/value/write",
                            json!({
                                "keyPath": format!("plugins.{}", plugin.id),
                                "value": { "enabled": enabled },
                                "mergeStrategy": "upsert"
                            }),
                        )
                        .await;
                    match result {
                        Ok(_) => {
                            state.push_notice(
                                BlockKind::System,
                                "✓ Plugin updated",
                                format!(
                                    "{} · {}",
                                    plugin.display_name,
                                    if enabled { "enabled" } else { "disabled" }
                                ),
                            );
                            let _ = refresh_integrations(server, state, true).await;
                        }
                        Err(error) => state.push_notice(
                            BlockKind::Error,
                            "플러그인 설정 실패",
                            error.to_string(),
                        ),
                    }
                }
                None => state.push_notice(
                    BlockKind::Error,
                    "플러그인을 찾을 수 없음",
                    format!("{query}\n/plugins에서 정확한 이름을 확인하세요."),
                ),
            },
            Err(error) => {
                state.push_notice(BlockKind::Error, "플러그인 조회 실패", error.to_string())
            }
        },
        Action::InstallPlugin(target) => {
            let response = server
                .request(
                    "plugin/install",
                    json!({
                        "pluginName": target.plugin_name,
                        "marketplacePath": target.marketplace_path,
                        "remoteMarketplaceName": target.remote_marketplace_name
                    }),
                )
                .await;
            match response {
                Ok(response) => {
                    let auth = format_apps_needing_auth(&response);
                    state.push_notice(
                        BlockKind::System,
                        "✓ Plugin installed",
                        if auth.is_empty() {
                            "새 세션부터 Skill과 MCP 도구를 사용할 수 있습니다.".to_owned()
                        } else {
                            format!(
                                "새 세션부터 사용할 수 있습니다.\n\n연결이 필요한 서비스:\n{auth}"
                            )
                        },
                    );
                    let _ = refresh_integrations(server, state, true).await;
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "플러그인 설치 실패", error.to_string())
                }
            }
        }
        Action::UninstallPlugin(target) => {
            match server
                .request("plugin/uninstall", json!({ "pluginId": target.plugin_id }))
                .await
            {
                Ok(_) => {
                    state.push_notice(
                        BlockKind::System,
                        "✓ Plugin uninstalled",
                        format!("{} · 새 세션부터 반영됩니다.", target.display_name),
                    );
                    let _ = refresh_integrations(server, state, true).await;
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "플러그인 제거 실패", error.to_string())
                }
            }
        }
        Action::ShowSkills => match list_skills(server, &state.cwd, true).await {
            Ok(response) => {
                state.update_skills(&response);
                state.push_notice(BlockKind::System, "Skills", format_skills(&response));
            }
            Err(error) => state.push_notice(BlockKind::Error, "Skill 목록 실패", error.to_string()),
        },
        Action::SetSkill { name, enabled } => match list_skills(server, &state.cwd, false).await {
            Ok(skills) => match resolve_skill(&skills, &name) {
                Some(skill) if skill.enabled == enabled => state.push_notice(
                    BlockKind::System,
                    "Skill unchanged",
                    format!(
                        "{} · already {}",
                        skill.name,
                        if enabled { "enabled" } else { "disabled" }
                    ),
                ),
                Some(skill) => match server
                    .request(
                        "skills/config/write",
                        json!({
                            "name": null,
                            "path": skill.path,
                            "enabled": enabled
                        }),
                    )
                    .await
                {
                    Ok(response) => {
                        let effective = response
                            .get("effectiveEnabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(enabled);
                        state.push_notice(
                            BlockKind::System,
                            "✓ Skill updated",
                            format!(
                                "{} · {}",
                                skill.name,
                                if effective { "enabled" } else { "disabled" }
                            ),
                        );
                        let _ = refresh_integrations(server, state, true).await;
                    }
                    Err(error) => {
                        state.push_notice(BlockKind::Error, "Skill 변경 실패", error.to_string())
                    }
                },
                None => state.push_notice(
                    BlockKind::Error,
                    "Skill을 찾을 수 없음",
                    format!("{name}\n/skills에서 정확한 이름을 확인하세요."),
                ),
            },
            Err(error) => state.push_notice(BlockKind::Error, "Skill 조회 실패", error.to_string()),
        },
        Action::ShowApps => match list_apps(server, &state.thread_id, false).await {
            Ok(response) => {
                state.update_apps(&response);
                state.push_notice(BlockKind::System, "Apps", format_apps(&response));
            }
            Err(error) => state.push_notice(BlockKind::Error, "App 목록 실패", error.to_string()),
        },
        Action::SetApp { query, enabled } => {
            match list_apps(server, &state.thread_id, true).await {
                Ok(response) => match resolve_app(&response, &query) {
                    Some(app) if app.enabled == enabled => state.push_notice(
                        BlockKind::System,
                        "App unchanged",
                        format!(
                            "{} · already {}",
                            app.name,
                            if enabled { "enabled" } else { "disabled" }
                        ),
                    ),
                    Some(app) => {
                        let base = app_config_base(&app.id);
                        let edits = if enabled {
                            vec![
                                json!({
                                    "keyPath": format!("{base}.enabled"),
                                    "value": null,
                                    "mergeStrategy": "replace"
                                }),
                                json!({
                                    "keyPath": format!("{base}.disabled_reason"),
                                    "value": null,
                                    "mergeStrategy": "replace"
                                }),
                            ]
                        } else {
                            vec![
                                json!({
                                    "keyPath": format!("{base}.enabled"),
                                    "value": false,
                                    "mergeStrategy": "replace"
                                }),
                                json!({
                                    "keyPath": format!("{base}.disabled_reason"),
                                    "value": "user",
                                    "mergeStrategy": "replace"
                                }),
                            ]
                        };
                        match server
                            .request(
                                "config/batchWrite",
                                json!({
                                    "edits": edits,
                                    "reloadUserConfig": true
                                }),
                            )
                            .await
                        {
                            Ok(_) => {
                                state.push_notice(
                                    BlockKind::System,
                                    "✓ App updated",
                                    format!(
                                        "{} · {}",
                                        app.name,
                                        if enabled { "enabled" } else { "disabled" }
                                    ),
                                );
                                let _ = refresh_integrations(server, state, true).await;
                            }
                            Err(error) => state.push_notice(
                                BlockKind::Error,
                                "App 설정 실패",
                                error.to_string(),
                            ),
                        }
                    }
                    None => state.push_notice(
                        BlockKind::Error,
                        "App을 찾을 수 없음",
                        format!("{query}\n/apps에서 정확한 이름을 확인하세요."),
                    ),
                },
                Err(error) => {
                    state.push_notice(BlockKind::Error, "App 조회 실패", error.to_string())
                }
            }
        }
        Action::RefreshSkills => match list_skills(server, &state.cwd, true).await {
            Ok(response) => state.update_skills(&response),
            Err(error) => {
                state.push_notice(BlockKind::Warning, "Skill 새로고침 실패", error.to_string())
            }
        },
        Action::OpenUrl(url) => {
            if let Err(error) = open_url(&url) {
                state.push_notice(BlockKind::Warning, "브라우저 열기 실패", error.to_string());
            }
        }
        Action::SetTheme(selected) => {
            renderer.set_theme(selected)?;
            if let Err(error) = theme::save(selected) {
                state.push_notice(BlockKind::Warning, "테마 저장 실패", error.to_string());
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

async fn list_skills(server: &AppServer, cwd: &str, force_reload: bool) -> Result<Value> {
    server
        .request(
            "skills/list",
            json!({
                "cwds": [cwd],
                "forceReload": force_reload
            }),
        )
        .await
}

async fn list_plugins(server: &AppServer, cwd: &str) -> Result<Value> {
    server
        .request(
            "plugin/list",
            json!({
                "cwds": [cwd]
            }),
        )
        .await
}

async fn list_apps(server: &AppServer, thread_id: &str, force_refetch: bool) -> Result<Value> {
    server
        .request(
            "app/list",
            json!({
                "cursor": null,
                "limit": 100,
                "threadId": thread_id,
                "forceRefetch": force_refetch
            }),
        )
        .await
}

async fn refresh_integrations(
    server: &AppServer,
    state: &mut AppState,
    force_reload: bool,
) -> Result<()> {
    let skills = list_skills(server, &state.cwd, force_reload).await;
    let plugins = server
        .request(
            "plugin/installed",
            json!({
                "cwds": [state.cwd]
            }),
        )
        .await;
    let apps = list_apps(server, &state.thread_id, force_reload).await;
    let mut errors = Vec::new();
    match skills {
        Ok(response) => state.update_skills(&response),
        Err(error) => errors.push(format!("Skill 조회 실패: {error}")),
    }
    match plugins {
        Ok(response) => state.update_plugins(&response),
        Err(error) => errors.push(format!("플러그인 조회 실패: {error}")),
    }
    match apps {
        Ok(response) => state.update_apps(&response),
        Err(error) => errors.push(format!("App 조회 실패: {error}")),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

struct ResolvedPlugin {
    id: String,
    name: String,
    display_name: String,
    marketplace_name: String,
    marketplace_path: Option<String>,
    remote_marketplace_name: Option<String>,
    description: Option<String>,
    installed: bool,
    enabled: bool,
    available: bool,
    toggle_allowed: bool,
    uninstall_allowed: bool,
    developer: Option<String>,
    capabilities: Vec<String>,
    website_url: Option<String>,
    privacy_policy_url: Option<String>,
    terms_of_service_url: Option<String>,
    must_show_interstitial: Option<bool>,
}

fn resolve_plugin(response: &Value, query: &str) -> Option<ResolvedPlugin> {
    let query = query.trim();
    let mut candidates = Vec::new();
    for marketplace in response
        .get("marketplaces")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let marketplace_name = marketplace.get("name")?.as_str()?;
        let marketplace_path = marketplace
            .get("path")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        for plugin in marketplace
            .get("plugins")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = plugin.get("name")?.as_str()?;
            let id = plugin.get("id")?.as_str()?;
            let display_name = plugin
                .get("interface")
                .and_then(|interface| interface.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or(name);
            if name.eq_ignore_ascii_case(query)
                || id.eq_ignore_ascii_case(query)
                || display_name.eq_ignore_ascii_case(query)
            {
                return Some(resolved_plugin(
                    plugin,
                    marketplace_name,
                    marketplace_path.clone(),
                ));
            }
            if name
                .to_ascii_lowercase()
                .contains(&query.to_ascii_lowercase())
                || display_name
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase())
            {
                candidates.push(resolved_plugin(
                    plugin,
                    marketplace_name,
                    marketplace_path.clone(),
                ));
            }
        }
    }
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn resolved_plugin(
    plugin: &Value,
    marketplace_name: &str,
    marketplace_path: Option<String>,
) -> ResolvedPlugin {
    let name = plugin
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("plugin")
        .to_owned();
    let display_name = plugin
        .get("interface")
        .and_then(|interface| interface.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_owned();
    let available = plugin.get("availability").and_then(Value::as_str) != Some("DISABLED_BY_ADMIN")
        && plugin.get("installPolicy").and_then(Value::as_str) != Some("NOT_AVAILABLE");
    let interface = plugin.get("interface");
    let installed = plugin
        .get("installed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ResolvedPlugin {
        id: plugin
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_owned(),
        name,
        display_name,
        marketplace_name: marketplace_name.to_owned(),
        remote_marketplace_name: marketplace_path
            .is_none()
            .then(|| marketplace_name.to_owned()),
        marketplace_path,
        description: interface
            .and_then(|interface| interface.get("shortDescription"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        installed,
        enabled: plugin
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        available,
        toggle_allowed: installed
            && plugin.get("installPolicy").and_then(Value::as_str) != Some("INSTALLED_BY_DEFAULT")
            && plugin.get("availability").and_then(Value::as_str) != Some("DISABLED_BY_ADMIN"),
        uninstall_allowed: installed
            && plugin.get("installPolicy").and_then(Value::as_str) != Some("INSTALLED_BY_DEFAULT"),
        developer: interface
            .and_then(|interface| interface.get("developerName"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        capabilities: interface
            .and_then(|interface| interface.get("capabilities"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        website_url: interface
            .and_then(|interface| interface.get("websiteUrl"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        privacy_policy_url: interface
            .and_then(|interface| interface.get("privacyPolicyUrl"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        terms_of_service_url: interface
            .and_then(|interface| interface.get("termsOfServiceUrl"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        must_show_interstitial: plugin
            .get("mustShowInstallationInterstitial")
            .and_then(Value::as_bool),
    }
}

fn plugin_install_disclosure(plugin: &ResolvedPlugin) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(developer) = plugin
        .developer
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("Developer: {developer}"));
    }
    if !plugin.capabilities.is_empty() {
        lines.push(format!("Capabilities: {}", plugin.capabilities.join(", ")));
    }
    if let Some(url) = plugin.website_url.as_deref() {
        lines.push(format!("Website: {url}"));
    }
    if let Some(url) = plugin.privacy_policy_url.as_deref() {
        lines.push(format!("Privacy: {url}"));
    }
    if let Some(url) = plugin.terms_of_service_url.as_deref() {
        lines.push(format!("Terms: {url}"));
    }
    if plugin.must_show_interstitial.is_none() {
        lines.push("설치 확인 정책이 제공되지 않아 안전하게 확인을 요구합니다.".to_owned());
    }
    lines
}

struct ResolvedApp {
    id: String,
    name: String,
    enabled: bool,
}

fn resolve_app(response: &Value, query: &str) -> Option<ResolvedApp> {
    let query = query.trim();
    let mut candidates = response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|app| {
            let id = app.get("id")?.as_str()?;
            let name = app.get("name").and_then(Value::as_str).unwrap_or(id);
            let exact = id.eq_ignore_ascii_case(query) || name.eq_ignore_ascii_case(query);
            let partial = id
                .to_ascii_lowercase()
                .contains(&query.to_ascii_lowercase())
                || name
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase());
            (exact || partial).then(|| {
                (
                    exact,
                    ResolvedApp {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        enabled: app
                            .get("isEnabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    if let Some(index) = candidates.iter().position(|(exact, _)| *exact) {
        return Some(candidates.swap_remove(index).1);
    }
    (candidates.len() == 1).then(|| candidates.remove(0).1)
}

fn app_config_base(id: &str) -> String {
    let quoted = serde_json::to_string(id).unwrap_or_else(|_| format!("\"{id}\""));
    format!("apps.{quoted}")
}

fn format_mcp_servers(response: &Value) -> String {
    let servers = response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if servers.is_empty() {
        return "연결된 MCP 서버가 없습니다.".to_owned();
    }
    servers
        .iter()
        .map(|server| {
            let name = server.get("name").and_then(Value::as_str).unwrap_or("MCP");
            let auth = server
                .get("authStatus")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let tools = server
                .get("tools")
                .and_then(Value::as_object)
                .map_or(0, |tools| tools.len());
            let login = if auth == "notLoggedIn" {
                format!(" · /mcp login {name}")
            } else {
                String::new()
            };
            format!("• {name} · {auth} · {tools} tools{login}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct ResolvedSkill {
    name: String,
    path: String,
    enabled: bool,
}

fn resolve_skill(response: &Value, query: &str) -> Option<ResolvedSkill> {
    let query = query.trim();
    let mut exact = Vec::new();
    let mut partial = Vec::new();
    for skill in response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .flat_map(|entry| {
            entry
                .get("skills")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
        })
    {
        let Some(name) = skill.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(path) = skill.get("path").and_then(Value::as_str) else {
            continue;
        };
        let resolved = || ResolvedSkill {
            name: name.to_owned(),
            path: path.to_owned(),
            enabled: skill
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        };
        if name.eq_ignore_ascii_case(query) || path.eq_ignore_ascii_case(query) {
            exact.push(resolved());
        } else if name
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
        {
            partial.push(resolved());
        }
    }
    if exact.len() == 1 {
        exact.pop()
    } else if exact.is_empty() && partial.len() == 1 {
        partial.pop()
    } else {
        None
    }
}

fn format_skills(response: &Value) -> String {
    let mut lines = Vec::new();
    for entry in response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        for skill in entry
            .get("skills")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = skill.get("name").and_then(Value::as_str).unwrap_or("skill");
            let enabled = skill
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let scope = skill.get("scope").and_then(Value::as_str).unwrap_or("user");
            let description = skill
                .get("interface")
                .and_then(|interface| interface.get("shortDescription"))
                .and_then(Value::as_str)
                .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                .unwrap_or_default();
            lines.push(format!(
                "{} ${name} · {scope}{}",
                if enabled { "✓" } else { "○" },
                if description.is_empty() {
                    String::new()
                } else {
                    format!(" — {description}")
                }
            ));
        }
        for error in entry
            .get("errors")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            lines.push(format!(
                "▲ {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Skill load error")
            ));
        }
    }
    if lines.is_empty() {
        "사용 가능한 Skill이 없습니다.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn format_plugins(response: &Value) -> String {
    let mut lines = Vec::new();
    for marketplace in response
        .get("marketplaces")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let marketplace_name = marketplace
            .get("interface")
            .and_then(|interface| interface.get("displayName"))
            .and_then(Value::as_str)
            .or_else(|| marketplace.get("name").and_then(Value::as_str))
            .unwrap_or("Marketplace");
        lines.push(format!("{marketplace_name}:"));
        for plugin in marketplace
            .get("plugins")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = plugin
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("plugin");
            let display_name = plugin
                .get("interface")
                .and_then(|interface| interface.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or(name);
            let installed = plugin
                .get("installed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let enabled = plugin
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let blocked =
                plugin.get("availability").and_then(Value::as_str) == Some("DISABLED_BY_ADMIN");
            let state = if blocked {
                "blocked"
            } else if installed && enabled {
                "installed"
            } else if installed {
                "disabled"
            } else {
                "available"
            };
            lines.push(format!(
                "  {} {display_name} · {name} · {state}",
                if installed { "✓" } else { "○" }
            ));
        }
    }
    for error in response
        .get("marketplaceLoadErrors")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        lines.push(format!(
            "▲ {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Marketplace load error")
        ));
    }
    if lines.is_empty() {
        "표시할 플러그인이 없습니다.".to_owned()
    } else {
        lines.join("\n")
    }
}

fn format_apps(response: &Value) -> String {
    let apps = response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if apps.is_empty() {
        return "표시할 App이 없습니다.".to_owned();
    }
    apps.iter()
        .map(|app| {
            let id = app.get("id").and_then(Value::as_str).unwrap_or("app");
            let name = app.get("name").and_then(Value::as_str).unwrap_or(id);
            let accessible = app
                .get("isAccessible")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let enabled = app
                .get("isEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let status = if !accessible {
                app.get("installUrl")
                    .and_then(Value::as_str)
                    .map_or("not connected".to_owned(), |url| {
                        format!("not connected · {url}")
                    })
            } else if enabled {
                "enabled".to_owned()
            } else {
                "disabled".to_owned()
            };
            format!(
                "{} {name} · {id} · {status}",
                if accessible && enabled { "✓" } else { "○" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_apps_needing_auth(response: &Value) -> String {
    response
        .get("appsNeedingAuth")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|app| {
            let name = app.get("name").and_then(Value::as_str).unwrap_or("App");
            app.get("installUrl")
                .and_then(Value::as_str)
                .map_or_else(|| format!("• {name}"), |url| format!("• {name}\n  {url}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(url);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .spawn()
        .with_context(|| format!("URL을 열지 못했습니다: {url}"))?;
    Ok(())
}

async fn start_turn(server: &AppServer, state: &mut AppState, text: String) {
    let input = state.turn_input(text);
    let params = json!({
        "threadId": state.thread_id,
        "input": input,
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

/// Re-reads the identity and entitlements after a login or an `account/updated`
/// notification. Best-effort: a failure leaves the previous values in place.
async fn refresh_account(server: &AppServer, state: &mut AppState) {
    if let Ok(label) = ensure_account(server).await {
        state.set_account(label);
    }
    state.set_account_plan(read_account_plan(server).await);
}

/// Plan and reset-credit entitlements for the welcome card. Fails soft: the panel
/// just shows placeholders when the server has nothing to report.
async fn read_account_plan(server: &AppServer) -> AccountPlan {
    server
        .request("account/rateLimits/read", json!({}))
        .await
        .map(|response| AccountPlan::from_rate_limits(&response))
        .unwrap_or_default()
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
        // The plan gets its own welcome row, so only the signed-in id is added here.
        Some("chatgpt") => match account.get("email").and_then(Value::as_str) {
            Some(email) if !email.is_empty() => format!("ChatGPT · {email}"),
            _ => "ChatGPT".to_owned(),
        },
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
    let _ = refresh_integrations(server, state, true).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_resolution_prefers_exact_identity_and_preserves_policy() {
        let response = json!({
            "marketplaces": [{
                "name": "openai-curated",
                "path": null,
                "plugins": [
                    {
                        "id": "calendar@openai-curated",
                        "name": "calendar",
                        "installed": true,
                        "enabled": false,
                        "installPolicy": "AVAILABLE",
                        "availability": "AVAILABLE",
                        "mustShowInstallationInterstitial": null,
                        "interface": {
                            "displayName": "Calendar",
                            "developerName": "Example",
                            "capabilities": ["events"]
                        }
                    },
                    {
                        "id": "calendar-notes@openai-curated",
                        "name": "calendar-notes",
                        "installed": false,
                        "enabled": false,
                        "installPolicy": "AVAILABLE",
                        "availability": "AVAILABLE"
                    }
                ]
            }]
        });

        let plugin = resolve_plugin(&response, "calendar").expect("exact plugin");
        assert_eq!(plugin.id, "calendar@openai-curated");
        assert!(plugin.installed);
        assert!(!plugin.enabled);
        assert!(plugin.toggle_allowed);
        assert_eq!(
            plugin.remote_marketplace_name.as_deref(),
            Some("openai-curated")
        );
        assert!(
            plugin_install_disclosure(&plugin)
                .iter()
                .any(|line| { line.contains("안전하게 확인") })
        );
    }

    #[test]
    fn app_resolution_rejects_ambiguous_partial_names_and_quotes_config_keys() {
        let response = json!({
            "data": [
                { "id": "calendar", "name": "Calendar", "isEnabled": true },
                { "id": "calendar-notes", "name": "Calendar Notes", "isEnabled": false }
            ]
        });

        assert!(resolve_app(&response, "cal").is_none());
        assert_eq!(
            resolve_app(&response, "calendar").expect("exact app").name,
            "Calendar"
        );
        assert_eq!(app_config_base("a.b"), "apps.\"a.b\"");
    }

    #[test]
    fn skill_resolution_uses_the_exact_path_when_names_collide() {
        let response = json!({
            "data": [{
                "skills": [
                    { "name": "review", "path": "C:/one/SKILL.md", "enabled": true },
                    { "name": "review", "path": "C:/two/SKILL.md", "enabled": false }
                ]
            }]
        });

        assert!(resolve_skill(&response, "review").is_none());
        let skill = resolve_skill(&response, "C:/two/SKILL.md").expect("path match");
        assert_eq!(skill.name, "review");
        assert!(!skill.enabled);
    }
}
