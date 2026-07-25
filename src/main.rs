mod app_server;
mod editor;
mod pricing;
mod renderer;
mod rollout;
mod state;
mod theme;
mod update;

use std::{
    env,
    future::Future,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use app_server::{AppServer, ServerEvent};
use arboard::Clipboard;
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Editor;
use futures_util::StreamExt;
use renderer::{BlockKind, Renderer, TerminalSession, View};
use serde_json::{Value, json};
use state::{
    AccountPlan, Action, AppState, LoginMethod, ModelInfo, SPINNER, SessionInfo, SessionPicker,
    SessionPickerResult, load_model_context_windows,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};

#[derive(Parser)]
#[command(
    name = "dvz",
    version,
    about = "Stable terminal UI for the official Codex app-server"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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

#[derive(clap::Subcommand)]
enum Command {
    /// Install the latest published release from npm.
    Update,
}

/// Native terminal selection happens outside crossterm, so watch the clipboard
/// and bridge successful copy-on-select changes back into the TUI notice.
struct ClipboardWatcher {
    clipboard: Option<Clipboard>,
    #[cfg(windows)]
    revision: Option<u32>,
    #[cfg(not(windows))]
    last_text: Option<String>,
}

impl ClipboardWatcher {
    fn new() -> Self {
        #[cfg(windows)]
        {
            Self {
                clipboard: Clipboard::new().ok(),
                revision: clipboard_win::seq_num().map(|revision| revision.get()),
            }
        }

        #[cfg(not(windows))]
        {
            let mut clipboard = Clipboard::new().ok();
            let last_text = clipboard
                .as_mut()
                .and_then(|clipboard| clipboard.get_text().ok());
            Self {
                clipboard,
                last_text,
            }
        }
    }

    fn copied_char_count(&mut self) -> Option<usize> {
        #[cfg(windows)]
        {
            let revision = clipboard_win::seq_num()?.get();
            if self.revision == Some(revision) {
                return None;
            }
            self.revision = Some(revision);
            return self
                .clipboard
                .as_mut()?
                .get_text()
                .ok()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().count());
        }

        #[cfg(not(windows))]
        {
            let text = self.clipboard.as_mut()?.get_text().ok()?;
            if self.last_text.as_deref() == Some(text.as_str()) {
                return None;
            }
            let count = text.chars().count();
            self.last_text = Some(text);
            (count > 0).then_some(count)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Update)) {
        return update::run_self_update();
    }
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
    // `thread/start` can take seconds, so the composer goes up first and the wait
    // happens behind a spinner instead of a blank terminal.
    let terminal = TerminalSession::enter()?;
    let mut renderer = Renderer::new(theme::current());
    renderer.clear_screen()?;
    let startup = await_startup(
        &mut renderer,
        start_or_resume_thread(
            server,
            is_resuming.then_some(resume_id.as_str()),
            model_override,
            cli.cwd.as_ref().map(|_| cwd.as_path()),
            &cwd,
        ),
        read_account_plan(server),
    )
    .await;
    let ui_result = match startup {
        Ok(Some(startup)) => {
            open_session(
                server,
                &mut renderer,
                startup,
                account,
                models,
                cli,
                &cwd,
                requested_model_name,
                is_resuming,
            )
            .await
        }
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    let _ = renderer.finish();
    drop(terminal);
    ui_result
}

/// What [`await_startup`] hands back once the slow launch requests land.
struct Startup {
    thread_response: Value,
    account_plan: AccountPlan,
    /// Whatever the user typed into the composer while waiting.
    typed: String,
}

/// Builds the session state from a resolved [`Startup`] and runs the UI. Split out
/// of [`run`] so every exit path still restores the terminal.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    server: &mut AppServer,
    renderer: &mut Renderer,
    startup: Startup,
    account: String,
    models: Vec<ModelInfo>,
    cli: &Cli,
    cwd: &Path,
    requested_model_name: String,
    is_resuming: bool,
) -> Result<()> {
    let Startup {
        thread_response,
        account_plan,
        typed,
    } = startup;
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
    state.set_account_plan(account_plan);
    if is_resuming {
        state.load_history(thread);
    }
    if !typed.is_empty() {
        state.handle_paste(&typed);
    }

    let (update_tx, update_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(latest) = update::check_for_update().await {
            let _ = update_tx.send(latest).await;
        }
    });

    // The real frame replaces the startup spinner before the slash-command
    // catalogue is fetched, so the wait never blocks a usable screen.
    renderer.clear_screen()?;
    draw(&mut state, renderer)?;
    let _ = refresh_integrations(server, &mut state, false).await;
    event_loop(server, &mut state, renderer, update_rx).await
}

/// Keeps the composer painted while the launch requests are in flight. Returns
/// `Ok(None)` when the user quits before the session is ready.
async fn await_startup(
    renderer: &mut Renderer,
    thread: impl Future<Output = Result<Value>>,
    plan: impl Future<Output = AccountPlan>,
) -> Result<Option<Startup>> {
    let mut editor = Editor::default();
    let mut events = EventStream::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(120));
    spinner_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let started = Instant::now();
    let mut frame = 0;
    let pending = async { tokio::join!(thread, plan) };
    tokio::pin!(pending);

    loop {
        renderer.render(
            &[],
            View {
                live_blocks: Vec::new(),
                overlay: None,
                editor: &editor,
                welcome: None,
                suggestions: Vec::new(),
                activity: Some(format!(
                    "{} 세션 준비 중… {}s",
                    SPINNER[frame],
                    started.elapsed().as_secs()
                )),
                footer: String::new(),
                status_line: None,
                composer_notice: None,
                composer_mode: None,
            },
        )?;

        tokio::select! {
            (thread_response, account_plan) = &mut pending => {
                return Ok(Some(Startup {
                    thread_response: thread_response?,
                    account_plan,
                    typed: editor.text(),
                }));
            }
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        if quits_startup(key) {
                            return Ok(None);
                        }
                        edit_while_waiting(&mut editor, key);
                    }
                    Some(Ok(Event::Paste(text))) => editor.insert_str(&text),
                    Some(Ok(Event::Resize(_, _))) => renderer.relayout()?,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(None),
                }
            }
            _ = spinner_tick.tick() => frame = (frame + 1) % SPINNER.len(),
        }
    }
}

fn quits_startup(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && matches!(key.code, KeyCode::Char('c' | 'd'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Composer editing available before the session exists: enough to start typing a
/// prompt, but no submit — there is nothing to submit to yet.
fn edit_while_waiting(editor: &mut Editor, key: KeyEvent) {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    match key.code {
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => editor.insert(ch),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Left => editor.move_left(),
        KeyCode::Right => editor.move_right(),
        KeyCode::Home => editor.move_home(),
        KeyCode::End => editor.move_end(),
        _ => {}
    }
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
            Some(Ok(Event::Resize(_, _))) => renderer.relayout()?,
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
    let mut clipboard_watcher = ClipboardWatcher::new();
    let mut resize = ResizeTracker::new();
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
                    Some(Ok(Event::Resize(columns, rows))) => {
                        resize.observe((columns, rows));
                        // The relayout lands on the settle tick, not here.
                        Action::Tick(false)
                    }
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
                let mut redraw = state.tick();
                if let Some(count) = clipboard_watcher.copied_char_count() {
                    state.set_copy_notice(count);
                    redraw = true;
                }
                // Ctrl+wheel font zoom changes the cell grid without always
                // sending a `Resize`, so the size is polled here as well.
                resize.observe(terminal_size());
                if resize.settled() {
                    renderer.relayout()?;
                    redraw = true;
                } else if resize.pending() {
                    // Nothing painted onto a grid that is still moving survives.
                    redraw = false;
                }
                Action::Tick(redraw)
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

/// Terminal geometry changes arrive in bursts: dragging a window edge, or
/// holding Ctrl+wheel to zoom the font, fires one event per step. Every one of
/// those steps leaves the terminal reflowing rows we wrapped ourselves, so
/// painting into them mid-drag only stacks debris. This holds the repaint until
/// the grid stops moving, then asks for exactly one exact layout.
struct ResizeTracker {
    size: (u16, u16),
    settle_at: Option<Instant>,
}

impl ResizeTracker {
    /// How still the grid has to be before it counts as the user having stopped.
    /// Short enough that a single resize still feels immediate.
    const SETTLE: Duration = Duration::from_millis(80);

    fn new() -> Self {
        Self {
            size: terminal_size(),
            settle_at: None,
        }
    }

    /// Records the geometry the terminal is at now, arming a relayout when it
    /// differs from the last one seen.
    fn observe(&mut self, size: (u16, u16)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.settle_at = Some(Instant::now() + Self::SETTLE);
    }

    /// True while a relayout is owed but the grid is still moving.
    fn pending(&self) -> bool {
        self.settle_at.is_some()
    }

    /// Fires once the grid has held still, so a drag is laid out at the size it
    /// ended on rather than at every step along the way.
    fn settled(&mut self) -> bool {
        let Some(deadline) = self.settle_at else {
            return false;
        };
        if Instant::now() < deadline {
            return false;
        }
        self.settle_at = None;
        true
    }
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((100, 30))
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
            // The app-server spends about five seconds inside `thread/start`,
            // and the event loop cannot redraw while this action is awaited,
            // so the notice has to be painted before the request goes out.
            state.set_waiting_notice("새 세션을 시작하는 중…");
            draw(state, renderer)?;
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
            // `thread/resume` is as slow as `thread/start`; see Action::NewThread.
            state.set_waiting_notice("세션을 불러오는 중…");
            draw(state, renderer)?;
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
            // `thread/resume` rebuilds the view from stored history, which knows
            // nothing about a turn still in flight. Carry it across by hand.
            let parent_turn = state.take_side_parent_turn();
            if let Some(parent_thread) = parent_thread {
                match resume_into_state(server, state, renderer, &parent_thread).await {
                    Ok(()) => {
                        state.restore_turn(parent_turn);
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
        Action::StartLogin(method) => {
            // The modal normally blocks input, so this only guards a stale login id.
            if let Some(login_id) = state.active_login_id().map(ToOwned::to_owned) {
                let _ = server
                    .request("account/login/cancel", json!({ "loginId": login_id }))
                    .await;
            }
            match server
                .request(
                    "account/login/start",
                    json!({ "type": method.param_type() }),
                )
                .await
            {
                Ok(response) => start_login_flow(state, method, &response),
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
        // Both /clear and Ctrl+L land here, so the welcome comes back either way.
        Action::ClearScreen => {
            state.reset_welcome();
            renderer.clear_screen()?;
        }
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
    // Independent lookups, so pay the slowest one instead of their sum.
    let (skills, plugins, apps) = tokio::join!(
        list_skills(server, &state.cwd, force_reload),
        server.request(
            "plugin/installed",
            json!({
                "cwds": [state.cwd]
            }),
        ),
        list_apps(server, &state.thread_id, force_reload),
    );
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

/// Moves `/login` into its waiting state from an `account/login/start` response.
/// The browser flow returns `authUrl`; the device flow returns a code to type.
fn start_login_flow(state: &mut AppState, method: LoginMethod, response: &Value) {
    let login_id = response.get("loginId").and_then(Value::as_str);
    match method {
        LoginMethod::Browser => {
            let auth_url = response.get("authUrl").and_then(Value::as_str);
            match (login_id, auth_url) {
                (Some(login_id), Some(auth_url)) => {
                    state.begin_login(login_id.to_owned(), auth_url.to_owned());
                    if let Err(error) = open_url(auth_url) {
                        state.push_notice(
                            BlockKind::Warning,
                            "브라우저 열기 실패",
                            format!("{error}\n위 Sign-in URL을 직접 열어주세요."),
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
        LoginMethod::DeviceCode => {
            let url = response.get("verificationUrl").and_then(Value::as_str);
            let user_code = response.get("userCode").and_then(Value::as_str);
            match (login_id, url, user_code) {
                (Some(login_id), Some(url), Some(user_code)) => state.begin_device_login(
                    login_id.to_owned(),
                    url.to_owned(),
                    user_code.to_owned(),
                ),
                _ => state.push_notice(
                    BlockKind::Error,
                    "로그인 실패",
                    "app-server가 verificationUrl 또는 userCode를 반환하지 않았습니다.",
                ),
            }
        }
    }
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

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Text typed while `thread/start` is still running has to survive into the
    /// session, otherwise the head start the spinner buys is worthless.
    #[test]
    fn typing_during_startup_is_kept_and_editable() {
        let mut editor = Editor::default();
        for ch in "hi!".chars() {
            edit_while_waiting(&mut editor, press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        edit_while_waiting(&mut editor, press(KeyCode::Backspace, KeyModifiers::NONE));

        assert_eq!(editor.text(), "hi");
    }

    /// Enter must not submit before a thread exists, and Ctrl+C still bails out.
    #[test]
    fn startup_composer_refuses_submit_but_honours_quit() {
        let mut editor = Editor::default();
        edit_while_waiting(&mut editor, press(KeyCode::Char('a'), KeyModifiers::NONE));
        edit_while_waiting(&mut editor, press(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(editor.text(), "a");
        assert!(quits_startup(press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!quits_startup(press(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

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

    #[test]
    fn a_resize_burst_lays_out_once_after_the_grid_stops_moving() {
        let mut resize = ResizeTracker::new();
        let (columns, rows) = resize.size;

        resize.observe((columns, rows));
        assert!(!resize.pending(), "the same grid is not a resize");

        resize.observe((columns - 1, rows));
        resize.observe((columns - 2, rows));
        assert!(resize.pending(), "a relayout is owed");
        assert!(!resize.settled(), "but not while the drag is still moving");

        std::thread::sleep(ResizeTracker::SETTLE + Duration::from_millis(20));

        assert!(resize.settled(), "the size the drag ended on is laid out");
        assert!(!resize.settled(), "and only once");
        assert!(!resize.pending());
    }
}
