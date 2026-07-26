mod app_server;
mod completion;
mod devezcode;
mod editor;
mod integrations;
mod paste;
mod pricing;
mod renderer;
mod rollout;
mod selection;
mod state;
mod theme;
mod update;

use std::{
    env,
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use app_server::{AppServer, ServerEvent};
use arboard::{Clipboard, ImageData};
use clap::Parser;
use completion::collect_workspace_entries;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use editor::Editor;
use futures_util::StreamExt;
use integrations::{McpServerInfo, PluginCatalog, PluginDetail, PluginInfo, PluginScope};
use paste::PasteBurst;
use renderer::{BlockKind, Pick, RenderMode, Renderer, SelectionResult, TerminalSession, View};
use rollout::Rollout;
use serde_json::{Value, json};
use state::{
    AccountPlan, Action, AppState, LoginMethod, ModelInfo, SessionInfo,
    SessionPicker, SessionPickerResult, load_model_context_windows,
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

    /// Renderer: fullscreen pins the composer and status line to the bottom and
    /// scrolls the transcript itself; inline hands the transcript to the
    /// terminal's own scrollback. Saved in %APPDATA%\DevezCLI\renderer.txt, or
    /// set DEVEZ_RENDERER.
    #[arg(long, value_name = "RENDERER")]
    renderer: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Install the latest published release from npm.
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Update)) {
        return update::run_self_update();
    }
    let selected_theme = theme::load(cli.theme.as_deref())?;
    theme::set_current(selected_theme);
    devezcode::init();
    let mut server = AppServer::spawn(&cli.codex).await?;

    let result = run(&cli, &mut server).await;
    server.shutdown().await;
    devezcode::finish();
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
        cli.model.clone()
    } else {
        Some(
            cli.model
                .clone()
                .unwrap_or_else(|| requested_model_name.clone()),
        )
    };

    // Everything the first frame shows — plan, cwd, account, model, limits, branch —
    // is already known here. Only the session id arrives late, so the state is built
    // now and the screen goes up before the slow `thread/start` round trip.
    let mut state = AppState::new(
        String::new(),
        cwd.to_string_lossy().into_owned(),
        account,
        models,
        &requested_model_name,
        cli.effort.as_deref(),
    );

    let render_mode = renderer::load_render_mode(cli.renderer.as_deref())?;
    let terminal = TerminalSession::enter(render_mode)?;
    let mut renderer = Renderer::new(theme::current(), render_mode);
    renderer.clear_screen()?;
    let ui_result = start_session(
        server,
        &mut state,
        &mut renderer,
        cli,
        &cwd,
        &resume_id,
        is_resuming,
        model_override.as_deref(),
        &requested_model_name,
    )
    .await;
    let _ = renderer.finish();
    drop(terminal);
    ui_result
}

/// Outcome of the pre-session loop.
enum Startup {
    Ready {
        thread_response: Value,
        /// A prompt submitted while the session was still starting.
        queued: Option<String>,
    },
    /// The user quit before the session existed.
    Quit,
}

/// Waits out `thread/start` with the UI already painted, then binds the session and
/// hands off to the event loop. Split out of [`run`] so every exit path — including
/// a failed handshake — still restores the terminal.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    server: &mut AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    cli: &Cli,
    cwd: &Path,
    resume_id: &str,
    is_resuming: bool,
    model_override: Option<&str>,
    requested_model_name: &str,
) -> Result<()> {
    // Kicked off now, alongside the `thread/start` request itself, so the 14
    // MB-worst-case parse runs under the spinner `await_thread` draws rather
    // than after it — starting it only once the request has already
    // returned would leave nothing left for it to run under.
    let rollout_handle = is_resuming.then(|| spawn_rollout_load(resume_id));
    let startup = await_thread(
        server,
        state,
        renderer,
        start_or_resume_thread(
            server,
            is_resuming.then_some(resume_id),
            model_override,
            cli.cwd.as_ref().map(|_| cwd),
            cwd,
        ),
        read_account_plan(server),
    )
    .await?;
    let Startup::Ready {
        thread_response,
        queued,
    } = startup
    else {
        return Ok(());
    };

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
        .unwrap_or(requested_model_name)
        .to_owned();
    let actual_effort = cli.effort.clone().or_else(|| {
        thread_response
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    validate_effort(state.models(), &actual_model, actual_effort.as_deref())?;
    let actual_cwd = thread_response
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| cwd.to_str().unwrap_or("."))
        .to_owned();

    state.attach_thread(
        thread_id,
        actual_cwd,
        &actual_model,
        actual_effort.as_deref(),
    );
    if is_resuming {
        let rollout = join_rollout_load(rollout_handle).await;
        state.load_history(thread, rollout.as_ref());
    }
    draw(state, renderer)?;

    let (update_tx, update_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(latest) = update::check_for_update().await {
            let _ = update_tx.send(latest).await;
        }
    });

    // Tool mentions resolve against the integration catalogues before a queued
    // prompt is sent. Filesystem results arrive independently in the event loop.
    let _ = refresh_integrations(server, state, false).await;
    if let Some(text) = queued {
        draw(state, renderer)?;
        start_turn(server, state, text).await;
    }
    event_loop(server, state, renderer, update_rx).await
}

/// Runs the full UI while `thread/start` is still in flight: the screen is live,
/// the composer accepts typing, and the account plan drops in as soon as it lands.
/// Used both at launch and by `/new`, so the wait always looks the same.
async fn await_thread(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread: impl Future<Output = Result<Value>>,
    plan: impl Future<Output = AccountPlan>,
) -> Result<Startup> {
    let mut events = EventStream::new();
    let mut paste_burst = PasteBurst::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(120));
    spinner_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut queued = None;
    let mut plan_pending = true;
    tokio::pin!(thread);
    tokio::pin!(plan);

    loop {
        draw(state, renderer)?;
        let action = tokio::select! {
            thread_response = &mut thread => {
                // In practice the plan lands first, but never drop it on the floor.
                if plan_pending {
                    state.set_account_plan(plan.as_mut().await);
                }
                return Ok(Startup::Ready {
                    thread_response: thread_response?,
                    queued,
                });
            }
            account_plan = &mut plan, if plan_pending => {
                plan_pending = false;
                state.set_account_plan(account_plan);
                Action::None
            }
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        renderer.clear_selection();
                        state.handle_key(paste_burst.observe(key, Instant::now()))
                    }
                    Some(Ok(Event::Mouse(mouse))) => renderer_mouse_action(renderer, &mouse, |pick| pick_action(state, pick)),
                    Some(Ok(Event::Paste(text))) => {
                        renderer.clear_selection();
                        if !attach_clipboard_image(state) {
                            state.handle_paste(&text);
                        }
                        Action::None
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        renderer.clear_selection();
                        renderer.relayout()?;
                        Action::None
                    }
                    Some(Ok(_)) => Action::None,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(Startup::Quit),
                }
            }
            _ = spinner_tick.tick() => Action::Tick(state.tick()),
        };

        match hold_until_thread(state, action, &mut queued) {
            // Listing sessions is the one server call the wait makes itself. It goes
            // straight to the RPC rather than through `execute_action`, which would
            // make the two mutually recursive — `/resume` waits the same way.
            Some(Action::OpenResume) => open_resume_picker(server, state).await,
            Some(action) => {
                if execute_local_action(state, renderer, action)? {
                    return Ok(Startup::Quit);
                }
            }
            None => {}
        }
    }
}

/// Splits actions into what can run before the session exists and what cannot.
/// A submitted prompt is queued rather than refused; anything else that needs the
/// thread gets a notice, because sending it now would target an empty thread id.
fn hold_until_thread(
    state: &mut AppState,
    action: Action,
    queued: &mut Option<String>,
) -> Option<Action> {
    // Callers only reach here mid-wait, but gate on the session itself rather than
    // on where the call sits, so a bound thread is never held back by mistake.
    if !state.thread_pending() {
        return Some(action);
    }
    match action {
        // Local UI work, safe with or without a session.
        action @ (Action::None
        | Action::Tick(_)
        | Action::Quit
        | Action::ClearScreen
        | Action::SetTheme(_)
        | Action::Copy(_)
        | Action::OpenUrl(_)) => Some(action),
        // Once a switch is owed, the prompt belongs to the session being resumed. The
        // session being started is about to be walked away from, and `prepare_resume`
        // forgets a turn id rather than interrupting it, so a turn begun here would
        // keep running with nothing watching it.
        Action::Submit(text) | Action::Steer(text) if state.has_deferred_resume() => {
            state.defer_prompt(&text);
            None
        }
        // The composer already committed the prompt and went busy, so hold the text
        // and start the turn the moment the thread lands.
        Action::Submit(text) => {
            *queued = Some(text);
            None
        }
        // A second prompt during the same wait joins the first: there is no turn to
        // steer yet, and dropping it would lose what the user typed.
        Action::Steer(text) => {
            match queued {
                Some(queued) => {
                    queued.push_str("\n\n");
                    queued.push_str(&text);
                }
                None => *queued = Some(text),
            }
            None
        }
        // Ctrl+C reads as taking back the prompt, not as taking back the switch, so
        // an owed resume survives it.
        Action::Interrupt => {
            *queued = None;
            state.cancel_deferred_prompt();
            state.cancel_queued_prompt();
            None
        }
        // The picker is built from `thread/list`; it never reads the thread being
        // started, so there is nothing to wait for before showing it.
        Action::OpenResume => Some(Action::OpenResume),
        // Switching away does need a bound thread, so the pick is held and replayed
        // once the session lands rather than refused.
        Action::ResumeThread(target) => {
            state.defer_resume(target);
            None
        }
        _ => {
            state.push_notice(
                BlockKind::Warning,
                "세션 준비 중",
                "세션이 준비된 뒤에 다시 실행해주세요.",
            );
            None
        }
    }
}

/// Kicks off the rollout parse right away — in parallel with whatever request
/// is about to ask the server for the thread — rather than after that request
/// resolves. The file runs to 14 MB, and the resume spinner has to keep
/// repainting while it is parsed; starting the parse only once the wait is
/// already over leaves nothing left for it to run under.
fn spawn_rollout_load(thread_id: &str) -> tokio::task::JoinHandle<Option<Rollout>> {
    let thread_id = thread_id.to_owned();
    tokio::spawn(async move {
        tokio::task::spawn_blocking(move || rollout::load(&state::codex_home()?, &thread_id))
            .await
            .ok()
            .flatten()
    })
}

/// Joins a rollout load kicked off by [`spawn_rollout_load`]. A panic in the
/// blocking read is treated the same as no rollout at all rather than
/// propagated — resuming with a plain transcript beats crashing over it.
async fn join_rollout_load(handle: Option<tokio::task::JoinHandle<Option<Rollout>>>) -> Option<Rollout> {
    match handle {
        Some(handle) => handle.await.unwrap_or(None),
        None => None,
    }
}

/// Fills the resume picker from `thread/list`. Shared so the picker looks the same
/// whether it is opened from a live session or from a session still starting up.
async fn open_resume_picker(server: &AppServer, state: &mut AppState) {
    match list_sessions(server, None, None, 100).await {
        Ok(sessions) => state.open_session_picker(sessions),
        Err(error) => state.push_notice(BlockKind::Error, "세션 목록 실패", error.to_string()),
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
            let mode = renderer::load_render_mode(cli.renderer.as_deref())?;
            choose_startup_session(sessions, cwd, mode).await
        }
        Some(target) => Ok(Some(
            resolve_session_target(server, target, Some(cwd)).await?,
        )),
    }
}

async fn choose_startup_session(
    sessions: Vec<SessionInfo>,
    cwd: &Path,
    mode: RenderMode,
) -> Result<Option<String>> {
    let terminal = TerminalSession::enter(mode)?;
    let mut renderer = Renderer::new(theme::current(), mode);
    renderer.clear_screen()?;
    let mut picker = SessionPicker::new(sessions, cwd.to_string_lossy().into_owned(), None);
    let editor = Editor::default();
    let mut events = EventStream::new();
    let mut paste_burst = PasteBurst::new();
    let mut composer_notice = None;

    let result = loop {
        renderer.render(
            &[],
            View {
                live_blocks: Vec::new(),
                overlay: Some(picker.overlay_view()),
                editor: &editor,
                composer_images: &[],
                welcome: None,
                suggestions: Vec::new(),
                activity: None,
                activity_phase: 0.0,
                footer: "Resume a Codex session".to_owned(),
                status_line: None,
                composer_notice: composer_notice.clone(),
                composer_mode: None,
            },
        )?;
        match events.next().await {
            Some(Ok(Event::Key(key))) => {
                renderer.clear_selection();
                composer_notice = None;
                match picker.handle_key(paste_burst.observe(key, Instant::now())) {
                    SessionPickerResult::None => {}
                    SessionPickerResult::Cancel => break Ok(None),
                    SessionPickerResult::Select(thread_id) => break Ok(Some(thread_id)),
                }
            }
            Some(Ok(Event::Mouse(mouse))) => {
                // A row here belongs to the picker itself, and the loop is what
                // gets to end on it, so the click is read back out rather than
                // acted on inside the closure.
                let mut clicked = None;
                let action = renderer_mouse_action(&mut renderer, &mouse, |pick| {
                    clicked = Some(pick);
                    Action::Tick(true)
                });
                if let Action::Copy(text) = action {
                    composer_notice = Some(
                        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(&text)) {
                            Ok(()) => format!("Copied {} chars to clipboard", text.chars().count()),
                            Err(error) => format!("복사 실패: {error}"),
                        },
                    );
                }
                match clicked {
                    Some(Pick::Row(row)) => {
                        composer_notice = None;
                        match picker.click_row(row) {
                            SessionPickerResult::None => {}
                            SessionPickerResult::Cancel => break Ok(None),
                            SessionPickerResult::Select(thread_id) => break Ok(Some(thread_id)),
                        }
                    }
                    // The mark on the panel's corner leaves the list the way Esc
                    // does: no session picked, and the session that would have
                    // started without `--resume` starts instead.
                    Some(Pick::Close) => break Ok(None),
                    _ => {}
                }
            }
            Some(Ok(Event::Paste(text))) => {
                renderer.clear_selection();
                composer_notice = None;
                picker.handle_paste(&text);
            }
            Some(Ok(Event::Resize(_, _))) => {
                renderer.clear_selection();
                renderer.relayout()?;
            }
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
    let mut paste_burst = PasteBurst::new();
    let mut activity_tick = tokio::time::interval(Duration::from_millis(120));
    activity_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut resize = ResizeTracker::new();
    let (workspace_tx, mut workspace_rx) = mpsc::channel(1);
    let mut indexed_cwd = None;
    draw(state, renderer)?;

    loop {
        // A session picked while the previous one was still starting is switched to
        // here. The event loop is the only place that can drive it: the wait it was
        // requested from cannot resume out of itself without recursing into another
        // wait, whereas this loop just comes back around.
        if let Some(deferred) = state.take_deferred_resume() {
            let should_quit = execute_action(
                server,
                state,
                renderer,
                Action::ResumeThread(deferred.target),
            )
            .await?;
            if should_quit {
                break;
            }
            // Sent after the switch settles, so it lands on the session now bound. A
            // failed resume rolls back to the one on screen, which is still the right
            // place for it.
            if let Some(text) = deferred.prompt {
                draw(state, renderer)?;
                send_queued_prompt(server, state, text).await;
            }
            draw(state, renderer)?;
            continue;
        }
        if indexed_cwd.as_deref() != Some(state.cwd.as_str()) {
            let cwd = state.cwd.clone();
            indexed_cwd = Some(cwd.clone());
            let tx = workspace_tx.clone();
            tokio::spawn(async move {
                let root = PathBuf::from(&cwd);
                let entries = tokio::task::spawn_blocking(move || collect_workspace_entries(&root))
                    .await
                    .unwrap_or_default();
                let _ = tx.send((cwd, entries)).await;
            });
        }
        let mut connection_closed = false;
        let action = tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        // Typing means the drag is over and its highlight is
                        // stale, so it goes before the key is acted on.
                        let cleared = renderer.clear_selection();
                        let key = paste_burst.observe(key, Instant::now());
                        let action = match scroll_request(renderer, &key) {
                            // A scroll moves the renderer's view, not the session, so
                            // it never reaches `handle_key` and cannot disturb a
                            // picker that wants the same key unshifted.
                            Some(delta) => Action::Tick(renderer.scroll(delta)),
                            None => state.handle_key(key),
                        };
                        match action {
                            Action::Tick(false) if cleared => Action::Tick(true),
                            action => action,
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => renderer_mouse_action(renderer, &mouse, |pick| pick_action(state, pick)),
                    Some(Ok(Event::Paste(text))) => {
                        renderer.clear_selection();
                        if !attach_clipboard_image(state) {
                            state.handle_paste(&text);
                        }
                        Action::None
                    }
                    Some(Ok(Event::Resize(columns, rows))) => {
                        renderer.clear_selection();
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
            Some((cwd, entries)) = workspace_rx.recv() => {
                if state.cwd == cwd {
                    state.update_workspace_entries(entries);
                }
                Action::None
            }
            _ = activity_tick.tick() => {
                let mut redraw = state.tick();
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

const WHEEL_ROWS: isize = 3;

#[derive(Debug, PartialEq, Eq)]
enum MouseRequest {
    Scroll(isize),
    SelectionStart(u16, u16),
    SelectionUpdate(u16, u16),
    SelectionEnd(u16, u16),
    CancelSelection,
    Hover(u16, u16),
    None,
}

/// Shift is the terminal's own escape hatch: holding it while dragging bypasses
/// mouse reporting in every terminal worth naming, so those events are left
/// alone and the user still gets native selection when they want it.
fn mouse_request(mouse: &MouseEvent) -> MouseRequest {
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        return match mouse.kind {
            MouseEventKind::ScrollUp => MouseRequest::Scroll(WHEEL_ROWS),
            MouseEventKind::ScrollDown => MouseRequest::Scroll(-WHEEL_ROWS),
            MouseEventKind::Down(MouseButton::Left)
            | MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Left) => MouseRequest::CancelSelection,
            _ => MouseRequest::None,
        };
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => MouseRequest::Scroll(WHEEL_ROWS),
        MouseEventKind::ScrollDown => MouseRequest::Scroll(-WHEEL_ROWS),
        MouseEventKind::Down(MouseButton::Left) => {
            MouseRequest::SelectionStart(mouse.column, mouse.row)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            MouseRequest::SelectionUpdate(mouse.column, mouse.row)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            MouseRequest::SelectionEnd(mouse.column, mouse.row)
        }
        MouseEventKind::Moved => MouseRequest::Hover(mouse.column, mouse.row),
        _ => MouseRequest::None,
    }
}

/// `on_pick` says what a click on a clickable cell means to the caller: the
/// session's own chrome and overlays for the event loop, the picker's own rows
/// for the standalone session picker, which runs before a session exists.
fn renderer_mouse_action(
    renderer: &mut Renderer,
    mouse: &MouseEvent,
    on_pick: impl FnOnce(Pick) -> Action,
) -> Action {
    match mouse_request(mouse) {
        MouseRequest::Scroll(delta) => {
            let cleared = renderer.clear_selection();
            Action::Tick(renderer.scroll(delta) || cleared)
        }
        MouseRequest::SelectionStart(column, row) => {
            Action::Tick(renderer.begin_selection(column, row))
        }
        MouseRequest::SelectionUpdate(column, row) => {
            Action::Tick(renderer.update_selection(column, row))
        }
        MouseRequest::SelectionEnd(column, row) => match renderer.finish_selection(column, row) {
            SelectionResult::Copy(text) => Action::Copy(text),
            // A press and release on the same cell never was a drag; tool
            // headings and the session's own chrome still want that click.
            SelectionResult::Click(column, row) => {
                // The down event painted a one-cell selection, so whatever the
                // click turns out to mean, the row has to be repainted.
                match renderer.pick_at(column, row) {
                    Some(pick) => match on_pick(pick) {
                        Action::Tick(_) => Action::Tick(true),
                        action => action,
                    },
                    None => {
                        renderer.toggle_tool_at(row);
                        Action::Tick(true)
                    }
                }
            }
            SelectionResult::None => Action::Tick(false),
        },
        MouseRequest::CancelSelection => Action::Tick(renderer.clear_selection()),
        MouseRequest::Hover(column, row) => Action::Tick(renderer.hover_at(column, row)),
        MouseRequest::None => Action::Tick(false),
    }
}

/// Clicking a reading on the chrome does what the key or command that owns that
/// setting does — nothing is duplicated here, so a badge and its shortcut can
/// never drift apart. Overlay picks belong to whoever painted the overlay.
fn pick_action(state: &mut AppState, pick: Pick) -> Action {
    match pick {
        Pick::PermissionMode => {
            state.cycle_permission_mode();
            Action::Tick(true)
        }
        Pick::FastMode => state.run_command("/fast"),
        Pick::Model => state.run_command("/model"),
        Pick::EffortSetting => state.run_command("/effort"),
        Pick::Close => state.close_overlay(),
        Pick::Row(index) => state.click_overlay_row(index),
        Pick::Effort(step) => state.click_effort_step(step),
    }
}

/// Maps a key to a transcript scroll, or `None` to let the session have it.
/// Shift is what keeps these out of the way: plain PageUp/PageDown already move
/// the cursor in the composer and the selection in every picker.
fn scroll_request(renderer: &Renderer, key: &KeyEvent) -> Option<isize> {
    if renderer.mode() != RenderMode::Fullscreen || !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::PageUp => Some(renderer.page_rows()),
        KeyCode::PageDown => Some(-renderer.page_rows()),
        KeyCode::Up => Some(1),
        KeyCode::Down => Some(-1),
        _ => None,
    }
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
        action @ (Action::None
        | Action::Tick(_)
        | Action::Copy(_)
        | Action::OpenUrl(_)
        | Action::SetTheme(_)
        | Action::ClearScreen
        | Action::Quit) => return execute_local_action(state, renderer, action),
        Action::Submit(text) => start_turn(server, state, text).await,
        Action::Steer(text) => {
            let Some(turn_id) = state.turn_id.clone() else {
                state.set_request_failed("활성 turn ID가 없어 추가 입력을 보낼 수 없습니다.");
                return Ok(false);
            };
            devezcode::note_prompt(&text);
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
        Action::NewThread => return start_new_thread(server, state, renderer).await,
        Action::OpenResume => open_resume_picker(server, state).await,
        Action::ResumeThread(target) => {
            return resume_thread(server, state, renderer, &target).await;
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
                match resume_into_state(server, state, renderer, &parent_thread).await? {
                    Switched::Done(queued) => {
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
                        return finish_thread_switch(server, state, renderer, queued).await;
                    }
                    Switched::Quit => return Ok(true),
                    // `resume_into_state` already posted the failure.
                    Switched::Failed => {}
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
        Action::OpenMcp(notice) => match list_mcp_servers(server, &state.thread_id).await {
            Ok(response) => {
                state.open_mcp_picker(McpServerInfo::list_from_value(&response), notice);
            }
            Err(error) => state.push_notice(BlockKind::Error, "MCP 목록 실패", error.to_string()),
        },
        Action::ReconnectMcp => {
            // `config/mcpServer/reload` re-reads the config and restarts every
            // server so the picker reflects the current connection state.
            match server.request("config/mcpServer/reload", json!({})).await {
                Ok(_) => {
                    let notice = Some("재연결했습니다.".to_owned());
                    match list_mcp_servers(server, &state.thread_id).await {
                        Ok(response) => {
                            state.open_mcp_picker(McpServerInfo::list_from_value(&response), notice)
                        }
                        Err(error) => state.push_notice(
                            BlockKind::Warning,
                            "재연결 후 조회 실패",
                            error.to_string(),
                        ),
                    }
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "MCP 재연결 실패", error.to_string())
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
        Action::OpenPlugins { scope, notice } => match list_plugins(server, &state.cwd).await {
            Ok(response) => {
                let catalog = PluginCatalog::from_value(&response);
                state.update_plugins(&response);
                state.open_plugin_picker(catalog, scope, notice);
            }
            Err(error) => {
                state.push_notice(BlockKind::Error, "플러그인 목록 실패", error.to_string())
            }
        },
        Action::OpenPluginDetail { target, origin } => {
            let params = if let Some(path) = target.marketplace_path.as_deref() {
                json!({
                    "pluginName": target.name,
                    "marketplacePath": path,
                    "cwds": [state.cwd]
                })
            } else {
                json!({
                    "pluginName": target.name,
                    "remoteMarketplaceName": target.remote_marketplace_name,
                    "cwds": [state.cwd]
                })
            };
            // The catalogue is refetched alongside the detail so the page shows
            // the plugin's current install state, not the state it had when the
            // list was drawn.
            let (detail, list) = tokio::join!(
                server.request("plugin/read", params),
                list_plugins(server, &state.cwd)
            );
            match (detail, list) {
                (Ok(detail), Ok(list)) => {
                    let catalog = PluginCatalog::from_value(&list);
                    state.open_plugin_detail(
                        catalog,
                        target,
                        PluginDetail::from_value(&detail),
                        origin,
                    );
                }
                (Err(error), _) | (_, Err(error)) => state.push_notice(
                    BlockKind::Error,
                    "플러그인 상세 조회 실패",
                    error.to_string(),
                ),
            }
        }
        Action::ConfirmPluginInstall(plugin) => state.confirm_plugin_install(&plugin),
        Action::ConfirmPluginUninstall(plugin) => state.confirm_plugin_uninstall(&plugin),
        Action::PreparePluginInstall(query) => match list_plugins(server, &state.cwd).await {
            Ok(response) => match PluginCatalog::from_value(&response).resolve(&query) {
                Some(plugin) if plugin.installed && !plugin.enabled => state.push_notice(
                    BlockKind::System,
                    "Already installed",
                    format!(
                        "{} · disabled\n/plugins enable {query}",
                        plugin.display_name
                    ),
                ),
                Some(plugin) if plugin.installed => state.push_notice(
                    BlockKind::System,
                    "Already installed",
                    plugin.display_name.clone(),
                ),
                Some(plugin) if !plugin.available => state.push_notice(
                    BlockKind::Error,
                    "설치할 수 없는 플러그인",
                    format!(
                        "{}은(는) 관리자 정책으로 비활성화되어 있습니다.",
                        plugin.display_name
                    ),
                ),
                Some(plugin) => state.confirm_plugin_install(plugin),
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
            Ok(response) => match PluginCatalog::from_value(&response).resolve(&query) {
                Some(plugin) if plugin.installed && !plugin.uninstall_allowed => state.push_notice(
                    BlockKind::Warning,
                    "제거할 수 없는 플러그인",
                    format!(
                        "{}은(는) 관리자에 의해 설치되었습니다.",
                        plugin.display_name
                    ),
                ),
                Some(plugin) if plugin.installed => state.confirm_plugin_uninstall(plugin),
                Some(plugin) => state.push_notice(
                    BlockKind::Warning,
                    "설치되지 않은 플러그인",
                    plugin.display_name.clone(),
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
            Ok(response) => match PluginCatalog::from_value(&response).resolve(&query) {
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
                    if let Err(error) = write_plugin_enabled(server, &plugin.id, enabled).await {
                        state.push_notice(
                            BlockKind::Error,
                            "플러그인 설정 실패",
                            error.to_string(),
                        );
                    } else {
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
        Action::SetPluginEnabled { plugin, enabled } => {
            match write_plugin_enabled(server, &plugin.id, enabled).await {
                Ok(_) => {
                    let _ = refresh_integrations(server, state, true).await;
                    reopen_plugins(
                        server,
                        state,
                        scope_of(&plugin),
                        format!(
                            "{} · {}",
                            plugin.display_name,
                            if enabled { "enabled" } else { "disabled" }
                        ),
                    )
                    .await;
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "플러그인 설정 실패", error.to_string())
                }
            }
        }
        Action::OpenMarketplaces(notice) => match list_plugins(server, &state.cwd).await {
            Ok(response) => {
                state.open_marketplace_picker(&PluginCatalog::from_value(&response), notice);
            }
            Err(error) => state.push_notice(
                BlockKind::Error,
                "마켓플레이스 조회 실패",
                error.to_string(),
            ),
        },
        Action::ConfirmMarketplaceAdd(source) => state.confirm_marketplace_add(source.trim()),
        Action::ConfirmMarketplaceRemove(name) => state.confirm_marketplace_remove(name.trim()),
        Action::AddMarketplace(source) => {
            // `owner/repo@ref` is the shorthand the Codex CLI accepts, and the
            // ref travels in its own field over the wire.
            let (source, ref_name) = split_marketplace_ref(&source);
            let mut params = json!({ "source": source });
            if let Some(ref_name) = ref_name {
                params["refName"] = Value::String(ref_name.to_owned());
            }
            match server.request("marketplace/add", params).await {
                Ok(response) => {
                    let already = response
                        .get("alreadyAdded")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let root = response
                        .get("installedRoot")
                        .and_then(Value::as_str)
                        .unwrap_or(source);
                    let _ = refresh_integrations(server, state, true).await;
                    reopen_marketplaces(
                        server,
                        state,
                        if already {
                            format!("이미 추가되어 있습니다 · {root}")
                        } else {
                            format!("추가했습니다 · {root}")
                        },
                    )
                    .await;
                }
                Err(error) => state.push_notice(
                    BlockKind::Error,
                    "마켓플레이스 추가 실패",
                    error.to_string(),
                ),
            }
        }
        Action::RemoveMarketplace(name) => {
            match server
                .request("marketplace/remove", json!({ "marketplaceName": name }))
                .await
            {
                Ok(_) => {
                    let _ = refresh_integrations(server, state, true).await;
                    reopen_marketplaces(server, state, format!("제거했습니다 · {name}")).await;
                }
                Err(error) => state.push_notice(
                    BlockKind::Error,
                    "마켓플레이스 제거 실패",
                    error.to_string(),
                ),
            }
        }
        Action::ReloadPlugins => {
            // Re-reading the catalogues is what makes new skills and mentions
            // usable; restarting the MCP servers is what makes a plugin's tools
            // usable. Neither implies the other, so a reload does both.
            let integrations = refresh_integrations(server, state, true).await;
            let reconnect = server.request("config/mcpServer/reload", json!({})).await;
            let servers = match &reconnect {
                Ok(_) => list_mcp_servers(server, &state.thread_id)
                    .await
                    .ok()
                    .map(|response| McpServerInfo::list_from_value(&response)),
                Err(_) => None,
            };
            let report = format_reload_report(
                integrations.as_ref().err().map(ToString::to_string),
                reconnect.as_ref().err().map(ToString::to_string),
                servers.as_deref(),
            );
            state.push_notice(
                if integrations.is_err() || reconnect.is_err() {
                    BlockKind::Warning
                } else {
                    BlockKind::System
                },
                "✓ Plugins reloaded",
                report,
            );
        }
        Action::UpgradeMarketplaces => {
            match server.request("marketplace/upgrade", json!({})).await {
                Ok(response) => {
                    let _ = refresh_integrations(server, state, true).await;
                    reopen_marketplaces(server, state, format_upgrade_result(&response)).await;
                }
                Err(error) => state.push_notice(
                    BlockKind::Error,
                    "마켓플레이스 갱신 실패",
                    error.to_string(),
                ),
            }
        }
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
                    // Skills and mentions come from the catalogue the refresh
                    // below re-reads, so they are live at once; a bundled MCP
                    // server only starts on a reconnect.
                    let base = "Skill과 멘션은 바로 사용할 수 있습니다.\n\
                                MCP 서버가 포함된 플러그인이면 /reload-plugins로 적용하세요.";
                    state.push_notice(
                        BlockKind::System,
                        "✓ Plugin installed",
                        if auth.is_empty() {
                            base.to_owned()
                        } else {
                            format!("{base}\n\n연결이 필요한 서비스:\n{auth}")
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
                        format!(
                            "{} · Skill과 멘션은 즉시 사라집니다.\n\
                             MCP 도구가 있었다면 /reload-plugins로 정리하세요.",
                            target.display_name
                        ),
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
        Action::RefreshSkills => match list_skills(server, &state.cwd, true).await {
            Ok(response) => state.update_skills(&response),
            Err(error) => {
                state.push_notice(BlockKind::Warning, "Skill 새로고침 실패", error.to_string())
            }
        },
        // The pick is already live for this session; this only makes it stick.
        Action::PersistModelDefault { model, effort } => {
            let writes = [("model", model), ("model_reasoning_effort", effort)];
            for (key, value) in writes {
                if let Err(error) = server
                    .request(
                        "config/value/write",
                        json!({ "keyPath": key, "value": value }),
                    )
                    .await
                {
                    state.push_notice(
                        BlockKind::Warning,
                        "기본값 저장 실패",
                        format!("{key}: {error}"),
                    );
                    break;
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
        } // Both /clear and Ctrl+L land here, so the welcome comes back either way.
    }
    Ok(false)
}

/// `/new`, wiped first and loaded after: the transcript clears and the fresh
/// welcome panel goes up immediately, then `thread/start` is waited out behind a
/// live screen exactly the way launch does it. Returns `true` when the user quits
/// during the wait.
async fn start_new_thread(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<bool> {
    // Read the request out of the old session before it is torn down.
    let params = json!({
        "cwd": state.cwd,
        "model": state.selected_model_name(),
        "serviceTier": state.service_tier(),
        "sessionStartSource": "clear",
        "threadSource": "devez-cli"
    });
    let previous_thread = state.thread_id.clone();

    renderer.clear_screen()?;
    state.prepare_new_thread();
    state.begin_thread_switch();

    let (response, queued) = match await_switch(
        server,
        state,
        renderer,
        previous_thread.clone(),
        server.request("thread/start", params),
    )
    .await?
    {
        Switch::Ready { response, queued } => (response, queued),
        Switch::Quit => return Ok(true),
        Switch::Failed => return Ok(false),
    };

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
    let (Some(thread_id), Some(cwd), Some(model)) = (thread_id, cwd, model) else {
        abandon_thread_switch(
            state,
            previous_thread,
            "thread/start 응답이 올바르지 않습니다.",
        );
        return Ok(false);
    };

    state.attach_thread(thread_id, cwd, &model, effort.as_deref());
    finish_thread_switch(server, state, renderer, queued).await
}

/// `/resume`, given the same treatment as [`start_new_thread`]: the screen resets
/// to a loading state straight away and the restored transcript arrives when
/// `thread/resume` answers. Returns `true` when the user quits during the wait.
async fn resume_thread(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    target: &str,
) -> Result<bool> {
    // Turning a name into an id is a quick lookup, and a miss has to leave the
    // current session untouched, so it runs before anything is torn down.
    let current_cwd = state.cwd.clone();
    let thread_id =
        match resolve_session_target(server, target, Some(Path::new(&current_cwd))).await {
            Ok(thread_id) => thread_id,
            Err(error) => {
                state.push_notice(BlockKind::Error, "세션 재개 실패", error.to_string());
                return Ok(false);
            }
        };

    match resume_into_state(server, state, renderer, &thread_id).await? {
        Switched::Done(queued) => finish_thread_switch(server, state, renderer, queued).await,
        Switched::Quit => Ok(true),
        Switched::Failed => Ok(false),
    }
}

/// A thread switch that has already been applied to `state`.
enum Switched {
    /// The session is bound; the payload is a prompt typed during the wait.
    Done(Option<String>),
    /// The user quit during the wait.
    Quit,
    /// Already reported and already rolled back.
    Failed,
}

/// Wipes the screen, resumes `thread_id` behind the loading spinner, and restores
/// its history. Shared by `/resume` and the return from a side conversation.
async fn resume_into_state(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread_id: &str,
) -> Result<Switched> {
    let previous_thread = state.thread_id.clone();
    renderer.clear_screen()?;
    state.prepare_resume();
    state.begin_thread_switch();

    // Started alongside the `thread/resume` request, not after it: `thread_id`
    // is already known, and the spinner `await_switch` draws is the only
    // thing left on screen for a 14 MB rollout parse to run under.
    let rollout_handle = spawn_rollout_load(thread_id);

    let (response, queued) = match await_switch(
        server,
        state,
        renderer,
        previous_thread.clone(),
        server.request("thread/resume", json!({ "threadId": thread_id })),
    )
    .await?
    {
        Switch::Ready { response, queued } => (response, queued),
        Switch::Quit => return Ok(Switched::Quit),
        Switch::Failed => return Ok(Switched::Failed),
    };

    let resumed = match parse_resumed_thread(&response) {
        Ok(resumed) => resumed,
        Err(error) => {
            abandon_thread_switch(state, previous_thread, error.to_string());
            return Ok(Switched::Failed);
        }
    };
    state.attach_thread(
        resumed.id,
        resumed.cwd,
        &resumed.model,
        resumed.effort.as_deref(),
    );
    let rollout = rollout_handle.await.unwrap_or(None);
    state.load_history(&resumed.thread, rollout.as_ref());
    Ok(Switched::Done(queued))
}

/// How a `/new` or `/resume` wait ended.
enum Switch {
    Ready {
        response: Value,
        /// A prompt submitted while the switch was in flight.
        queued: Option<String>,
    },
    /// The user quit during the wait.
    Quit,
    /// The request failed; already reported and already rolled back.
    Failed,
}

/// Shared wait for `/new` and `/resume`. Refreshes the account plan alongside the
/// switch — it is far quicker, so the credits land on the new screen long before
/// the session does.
async fn await_switch(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    previous_thread: String,
    request: impl Future<Output = Result<Value>>,
) -> Result<Switch> {
    match await_thread(server, state, renderer, request, read_account_plan(server)).await {
        Ok(Startup::Ready {
            thread_response,
            queued,
        }) => Ok(Switch::Ready {
            response: thread_response,
            queued,
        }),
        Ok(Startup::Quit) => Ok(Switch::Quit),
        Err(error) => {
            abandon_thread_switch(state, previous_thread, error.to_string());
            Ok(Switch::Failed)
        }
    }
}

/// Reports a failed switch and puts the session back on the thread it left, so an
/// error never strands the UI waiting for a thread that will never arrive.
fn abandon_thread_switch(
    state: &mut AppState,
    previous_thread_id: String,
    message: impl Into<String>,
) {
    state.cancel_thread_switch(previous_thread_id);
    state.set_request_failed(message);
}

/// Tail shared by `/new` and `/resume`: paint the bound session, reload the
/// catalogues, then send whatever was typed during the wait.
async fn finish_thread_switch(
    server: &AppServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    queued: Option<String>,
) -> Result<bool> {
    draw(state, renderer)?;
    // Tool mentions resolve before a queued prompt is sent. The event loop
    // notices cwd changes and refreshes filesystem results independently.
    let _ = refresh_integrations(server, state, true).await;
    if let Some(text) = queued {
        draw(state, renderer)?;
        send_queued_prompt(server, state, text).await;
    }
    Ok(false)
}

/// Sends a prompt typed during a switch. Returning from a side conversation can
/// bring a turn back with it, so the prompt joins that turn rather than starting a
/// competing one.
async fn send_queued_prompt(server: &AppServer, state: &mut AppState, text: String) {
    let Some(turn_id) = state.turn_id.clone() else {
        start_turn(server, state, text).await;
        return;
    };
    devezcode::note_prompt(&text);
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

/// The fields `/resume` needs out of a `thread/resume` response.
struct ResumedThread {
    thread: Value,
    id: String,
    cwd: String,
    model: String,
    effort: Option<String>,
}

fn parse_resumed_thread(response: &Value) -> Result<ResumedThread> {
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
    Ok(ResumedThread {
        thread,
        id,
        cwd,
        model,
        effort,
    })
}

/// Actions that touch nothing but the terminal and in-memory state. Shared so a
/// thread-start wait can honour them without re-entering [`execute_action`] — the
/// two would otherwise be mutually recursive, since `/new` waits the same way.
fn execute_local_action(
    state: &mut AppState,
    renderer: &mut Renderer,
    action: Action,
) -> Result<bool> {
    match action {
        Action::Quit => return Ok(true),
        Action::Copy(text) => {
            match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(&text)) {
                Ok(()) => state.set_copy_notice(text.chars().count()),
                Err(error) => state.push_notice(BlockKind::Error, "복사 실패", error.to_string()),
            }
        }
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
        Action::ClearScreen => {
            state.reset_welcome();
            renderer.clear_screen()?;
        }
        // `Action::None`, `Action::Tick`, and anything routed here by mistake: the
        // callers only ever pass the variants above, so silently doing nothing is
        // safer than panicking inside the render loop.
        _ => {}
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

async fn list_mcp_servers(server: &AppServer, thread_id: &str) -> Result<Value> {
    server
        .request(
            "mcpServerStatus/list",
            json!({
                "threadId": thread_id,
                "detail": "toolsAndAuthOnly",
                "limit": 100
            }),
        )
        .await
}

async fn write_plugin_enabled(server: &AppServer, plugin_id: &str, enabled: bool) -> Result<Value> {
    server
        .request(
            "config/value/write",
            json!({
                "keyPath": format!("plugins.{plugin_id}"),
                "value": { "enabled": enabled },
                "mergeStrategy": "upsert"
            }),
        )
        .await
}

/// The list a plugin action should return to once the catalogue is refetched.
fn scope_of(plugin: &PluginInfo) -> Option<PluginScope> {
    Some(PluginScope::Marketplace(plugin.marketplace_name.clone()))
}

/// Refetches the catalogue and reopens the plugin picker where it was, so an
/// action's result is visible in the list it was taken from.
async fn reopen_plugins(
    server: &AppServer,
    state: &mut AppState,
    scope: Option<PluginScope>,
    notice: String,
) {
    match list_plugins(server, &state.cwd).await {
        Ok(response) => {
            let catalog = PluginCatalog::from_value(&response);
            state.update_plugins(&response);
            state.open_plugin_picker(catalog, scope, Some(notice));
        }
        Err(error) => state.push_notice(
            BlockKind::Warning,
            "플러그인 목록 새로고침 실패",
            format!("{notice}\n{error}"),
        ),
    }
}

async fn reopen_marketplaces(server: &AppServer, state: &mut AppState, notice: String) {
    match list_plugins(server, &state.cwd).await {
        Ok(response) => {
            state.open_marketplace_picker(&PluginCatalog::from_value(&response), Some(notice));
        }
        Err(error) => state.push_notice(
            BlockKind::Warning,
            "마켓플레이스 새로고침 실패",
            format!("{notice}\n{error}"),
        ),
    }
}

/// Splits the `owner/repo@ref` shorthand the Codex CLI accepts. An `@` inside a
/// scheme-bearing URL or a Windows path is not a ref separator, so only the
/// trailing segment of a bare `owner/repo` form is treated as one.
fn split_marketplace_ref(source: &str) -> (&str, Option<&str>) {
    let source = source.trim();
    if source.contains("://") || source.starts_with('.') || source.contains('\\') {
        return (source, None);
    }
    match source.rsplit_once('@') {
        // `git@github.com:owner/repo` is an SSH URL, not a ref.
        Some((head, tail)) if !head.is_empty() && !tail.is_empty() && !tail.contains(':') => {
            (head, Some(tail))
        }
        _ => (source, None),
    }
}

/// Names what a reload changed, one line per catalogue. Kept pure so the
/// wording is testable without a server, and so a partial failure still reports
/// whatever did succeed.
/// A successful reload says nothing beyond its title: counts and deltas are
/// noise the user did not ask for. Only what still needs attention is reported.
fn format_reload_report(
    integrations_error: Option<String>,
    reconnect_error: Option<String>,
    servers: Option<&[McpServerInfo]>,
) -> String {
    let mut lines = Vec::new();
    if let Some(error) = reconnect_error {
        lines.push(format!("MCP 재연결 실패 · {error}"));
    } else if let Some(servers) = servers {
        let needs_login = servers
            .iter()
            .filter(|server| server.needs_login())
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>();
        if !needs_login.is_empty() {
            lines.push(format!("로그인 필요: {}", needs_login.join(", ")));
        }
    }
    if let Some(error) = integrations_error {
        lines.push(format!("일부 목록을 갱신하지 못했습니다 · {error}"));
    }
    lines.join("\n")
}

/// Reports what the server says it did, not what was asked of it. The response
/// echoes the marketplaces it considered in `selectedMarketplaces`, which is the
/// only trustworthy account of the upgrade's scope.
fn format_upgrade_result(response: &Value) -> String {
    let strings = |key: &str| {
        response
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
    };
    let considered = strings("selectedMarketplaces");
    let upgraded = strings("upgradedRoots").len();
    let errors = response
        .get("errors")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|error| {
            error
                .get("message")
                .or_else(|| error.get("error"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();

    let scope = if considered.is_empty() {
        "Git 마켓플레이스".to_owned()
    } else {
        considered.join(", ")
    };
    if !errors.is_empty() {
        return format!("{scope} · {upgraded}개 갱신 · 실패: {}", errors.join("; "));
    }
    if upgraded == 0 {
        return format!("{scope} · 이미 최신 상태입니다.");
    }
    format!("{scope} · {upgraded}개를 갱신했습니다.")
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
    devezcode::note_prompt(&text);
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

fn attach_clipboard_image(state: &mut AppState) -> bool {
    let Ok(mut clipboard) = Clipboard::new() else {
        return false;
    };
    let Ok(image) = clipboard.get_image() else {
        return false;
    };
    let Ok(path) = write_clipboard_bmp(&image) else {
        return false;
    };
    state.attach_local_image(path.to_string_lossy().into_owned());
    true
}

fn write_clipboard_bmp(image: &ImageData<'_>) -> std::io::Result<PathBuf> {
    let directory = env::temp_dir().join("devez-cli-images");
    fs::create_dir_all(&directory)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = directory.join(format!("clipboard-{stamp}.bmp"));
    let pixel_bytes = image.width.saturating_mul(image.height).saturating_mul(4);
    let file_size = 54usize.saturating_add(pixel_bytes).min(u32::MAX as usize) as u32;
    let mut bmp = Vec::with_capacity(file_size as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(image.width as u32).to_le_bytes());
    bmp.extend_from_slice(&(image.height as u32).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    for row in (0..image.height).rev() {
        for rgba in image.bytes[row * image.width * 4..(row + 1) * image.width * 4].chunks_exact(4) {
            bmp.extend_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
    fs::write(&path, bmp)?;
    Ok(path)
}

fn draw(state: &mut AppState, renderer: &mut Renderer) -> Result<()> {
    // Every state change the user can see reaches a frame, so the host's copy of
    // the session state is refreshed from the same place rather than from each
    // of the call sites that can move it.
    devezcode::sync(&state.thread_id, state.busy, state.awaiting_input());
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
        // The plan gets its own welcome row, so only the signed-in id is added
        // here. An email already says this is the OAuth sign-in rather than a key
        // or a Bedrock role, so it stands alone — every session here runs against
        // Codex, which makes naming the provider a word that never varies.
        Some("chatgpt") => match account.get("email").and_then(Value::as_str) {
            Some(email) if !email.is_empty() => email.to_owned(),
            // With no address to show, the sign-in method is all that is left.
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use state::EffortInfo;

    use super::*;

    fn starting_state() -> AppState {
        AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            Vec::new(),
            "gpt-5.6-sol",
            None,
        )
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// Clicking the badge has to land on the same code path Shift+Tab does, or
    /// the two ways of switching mode would drift apart.
    #[test]
    fn clicking_the_permission_mode_badge_cycles_the_mode() {
        let mut state = starting_state();
        let first = state.permission_mode();

        let action = pick_action(&mut state, Pick::PermissionMode);

        assert!(matches!(action, Action::Tick(true)));
        assert_ne!(state.permission_mode(), first);
        // The same cycle the key walks: one click moves exactly one step.
        state.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT));
        let by_key = state.permission_mode();
        pick_action(&mut state, Pick::PermissionMode);
        assert_ne!(state.permission_mode(), by_key);
    }

    /// The effort picker only has rows once a model has published its tiers, so
    /// the state a reading is clicked on carries one.
    fn state_with_a_model() -> AppState {
        AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![ModelInfo {
                id: "gpt-5.6-sol".to_owned(),
                model: "gpt-5.6-sol".to_owned(),
                display_name: "GPT-5.6 Sol".to_owned(),
                efforts: ["low", "high"]
                    .into_iter()
                    .map(|id| EffortInfo { id: id.to_owned() })
                    .collect(),
                default_effort: "high".to_owned(),
                is_default: true,
                context_window: None,
                fast_service_tier: Some("priority".to_owned()),
            }],
            "gpt-5.6-sol",
            None,
        )
    }

    /// The model and effort readings stand for the commands that change them, so a
    /// click opens the very picker `/model` and `/effort` open.
    #[test]
    fn clicking_the_status_readings_opens_the_matching_picker() {
        let mut state = state_with_a_model();

        pick_action(&mut state, Pick::Model);
        assert!(state.view().overlay.is_some(), "the model picker is open");

        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::EffortSetting);
        assert!(state.view().overlay.is_some(), "the effort picker is open");
    }

    /// Fast is a toggle, not a picker: the click flips the tier the same way
    /// `/fast` does, and reports it so the badge repaints.
    #[test]
    fn clicking_the_fast_badge_toggles_the_service_tier() {
        let mut state = state_with_a_model();

        assert!(matches!(
            pick_action(&mut state, Pick::FastMode),
            Action::SetFast(true)
        ));
    }

    /// An open picker is no reason to make the other reading dead: clicking it
    /// switches straight over, which is how one gets from `/model` to `/effort`
    /// without touching the keyboard.
    #[test]
    fn the_readings_switch_between_the_two_open_pickers() {
        let mut state = state_with_a_model();

        pick_action(&mut state, Pick::Model);
        assert_eq!(state.view().overlay.map(|overlay| overlay.title), Some("Model".to_owned()));

        pick_action(&mut state, Pick::EffortSetting);
        assert_eq!(state.view().overlay.map(|overlay| overlay.title), Some("Effort".to_owned()));

        pick_action(&mut state, Pick::Model);
        assert_eq!(state.view().overlay.map(|overlay| overlay.title), Some("Model".to_owned()));
    }

    /// The permission mode is not what either picker is asking about, so the badge
    /// stays inert until the question on screen is answered — as it is for keys.
    #[test]
    fn an_open_picker_still_swallows_the_mode_badge() {
        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::Model);
        let mode = state.permission_mode();

        pick_action(&mut state, Pick::PermissionMode);

        assert_eq!(state.permission_mode(), mode);
    }

    /// Clicking a model row does what typing its number does: takes the model and
    /// moves on to how long the pick should last.
    #[test]
    fn clicking_a_model_row_carries_on_to_the_scope_question() {
        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::Model);

        pick_action(&mut state, Pick::Row(0));

        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Apply to".to_owned())
        );

        // The summary and the blank above the choices are not choices.
        pick_action(&mut state, Pick::Row(0));
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Apply to".to_owned())
        );

        // The first choice sits under those two rows, and applies the pick.
        pick_action(&mut state, Pick::Row(2));
        assert!(state.view().overlay.is_none(), "the pick was applied");
    }

    /// The track beside the model list is a control, so a click only moves it. The
    /// effort picker has nothing else to answer for, so a click there settles it.
    #[test]
    fn clicking_an_effort_step_adjusts_in_one_picker_and_applies_in_the_other() {
        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::Model);

        pick_action(&mut state, Pick::Effort(0));
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Model".to_owned()),
            "the model picker stays open"
        );

        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::EffortSetting);
        pick_action(&mut state, Pick::Effort(0));

        assert!(state.view().overlay.is_none(), "the effort picker closed");
        assert!(state.view().status_line.is_some_and(|status| status.effort == "low"));
    }

    /// Every panel that paints the mark answers for it, and nothing else does.
    #[test]
    fn the_mark_closes_the_pickers_that_paint_it() {
        for pick in [Pick::Model, Pick::EffortSetting] {
            let mut state = state_with_a_model();
            pick_action(&mut state, pick);
            let overlay = state.view().overlay.expect("a picker is open");
            assert!(overlay.closable, "it paints the mark");

            pick_action(&mut state, Pick::Close);

            assert!(state.view().overlay.is_none(), "the mark closed it");
        }

        // The `Apply to` step is the same flow, so it closes the same way.
        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::Model);
        pick_action(&mut state, Pick::Row(0));
        assert!(state.view().overlay.is_some_and(|overlay| overlay.closable));
        pick_action(&mut state, Pick::Close);
        assert!(state.view().overlay.is_none());
    }

    /// With nothing open the mark is not painted anywhere, so a click that claims
    /// to be on it changes nothing.
    #[test]
    fn closing_with_nothing_open_does_nothing() {
        let mut state = starting_state();

        assert!(matches!(
            pick_action(&mut state, Pick::Close),
            Action::Tick(false)
        ));
    }

    /// With nothing open there is no overlay to answer for a row click.
    #[test]
    fn overlay_picks_do_nothing_with_no_overlay_open() {
        let mut state = starting_state();

        assert!(matches!(
            pick_action(&mut state, Pick::Row(3)),
            Action::Tick(false)
        ));
        assert!(matches!(
            pick_action(&mut state, Pick::Effort(1)),
            Action::Tick(false)
        ));
    }

    /// A prompt sent before `thread/start` answers must not be dropped: it is held
    /// and replayed, so the head start the painted screen buys is real.
    #[test]
    fn a_prompt_submitted_during_startup_is_queued_not_lost() {
        let mut state = starting_state();
        let mut queued = None;
        assert!(state.thread_pending());

        state.handle_paste("hello");
        let action = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        assert!(hold_until_thread(&mut state, action, &mut queued).is_none());
        assert_eq!(queued.as_deref(), Some("hello"));
        assert!(state.busy, "the composer already showed the prompt as sent");
        // The wait itself is silent, but a prompt sent into it is not: the user gets
        // the same `Working` line they would for any other turn.
        assert!(
            state
                .view()
                .activity
                .as_deref()
                .is_some_and(|activity| activity.contains("Working (")),
            "a queued prompt still reports as working"
        );
    }

    /// The second Enter cannot steer a turn that has not started, so it joins the
    /// queued prompt instead of erroring against an empty thread id.
    #[test]
    fn a_second_prompt_during_startup_joins_the_queued_one() {
        let mut state = starting_state();
        let mut queued = None;

        for prompt in ["first", "second"] {
            state.handle_paste(prompt);
            let action = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
            hold_until_thread(&mut state, action, &mut queued);
        }

        assert_eq!(queued.as_deref(), Some("first\n\nsecond"));
    }

    /// Ctrl+C over a queued prompt reads like interrupting a live turn.
    #[test]
    fn interrupting_startup_drops_the_queued_prompt() {
        let mut state = starting_state();
        let mut queued = None;
        state.handle_paste("hello");
        let submit = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        hold_until_thread(&mut state, submit, &mut queued);

        let interrupt = state.handle_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(hold_until_thread(&mut state, interrupt, &mut queued).is_none());

        assert_eq!(queued, None);
        assert!(!state.busy);
    }

    /// Local UI work stays available during the wait; thread-bound work is not sent
    /// against a session that does not exist yet.
    #[test]
    fn startup_allows_local_actions_and_defers_thread_bound_ones() {
        let mut state = starting_state();
        let mut queued = None;

        assert!(hold_until_thread(&mut state, Action::ClearScreen, &mut queued).is_some());
        assert!(hold_until_thread(&mut state, Action::Quit, &mut queued).is_some());
        assert!(hold_until_thread(&mut state, Action::Compact, &mut queued).is_none());
        assert!(hold_until_thread(&mut state, Action::ShowDiff, &mut queued).is_none());
    }

    /// The resume picker reads `thread/list`, not the thread being started, so
    /// `/resume` right after launch opens it instead of asking the user to retry.
    #[test]
    fn resume_opens_the_picker_while_the_session_is_still_starting() {
        let mut state = starting_state();
        let mut queued = None;

        let held = hold_until_thread(&mut state, Action::OpenResume, &mut queued);

        assert!(
            matches!(held, Some(Action::OpenResume)),
            "the picker request is passed through to be listed"
        );
        assert!(
            state.drain_committed().is_empty(),
            "no `session not ready` notice is raised"
        );
    }

    /// Picking a session mid-wait cannot run `thread/resume` yet — the thread being
    /// started is not there to switch away from — so it is held and replayed by the
    /// event loop the moment the session binds.
    #[test]
    fn a_session_picked_during_startup_is_resumed_once_the_thread_lands() {
        let mut state = starting_state();
        let mut queued = None;

        let held = hold_until_thread(
            &mut state,
            Action::ResumeThread("thread-9".to_owned()),
            &mut queued,
        );

        assert!(held.is_none(), "the switch waits for the session");
        assert!(state.drain_committed().is_empty(), "the wait is silent");

        let deferred = state.take_deferred_resume().expect("the switch is owed");
        assert_eq!(deferred.target, "thread-9");
        assert!(
            !state.has_deferred_resume(),
            "the held switch runs exactly once"
        );
    }

    /// A prompt typed after a session was picked must not start a turn on the session
    /// being left: `prepare_resume` forgets the turn id without interrupting it, so
    /// that turn would run on server-side with nothing watching it. It rides along.
    #[test]
    fn a_prompt_typed_after_picking_a_session_travels_with_the_switch() {
        let mut state = starting_state();
        let mut queued = None;
        hold_until_thread(
            &mut state,
            Action::ResumeThread("thread-9".to_owned()),
            &mut queued,
        );

        for prompt in ["first", "second"] {
            state.handle_paste(prompt);
            let action = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
            hold_until_thread(&mut state, action, &mut queued);
        }

        assert_eq!(
            queued, None,
            "the session being started is never given the prompt"
        );
        let deferred = state.take_deferred_resume().expect("the switch is owed");
        assert_eq!(deferred.target, "thread-9");
        assert_eq!(deferred.prompt.as_deref(), Some("first\n\nsecond"));
    }

    /// Choosing again before the session lands only changes where the prompt goes.
    #[test]
    fn repicking_a_session_keeps_the_prompt_typed_for_the_switch() {
        let mut state = starting_state();
        let mut queued = None;
        hold_until_thread(
            &mut state,
            Action::ResumeThread("thread-9".to_owned()),
            &mut queued,
        );
        state.handle_paste("hello");
        let submit = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        hold_until_thread(&mut state, submit, &mut queued);

        hold_until_thread(
            &mut state,
            Action::ResumeThread("thread-4".to_owned()),
            &mut queued,
        );

        let deferred = state.take_deferred_resume().expect("the switch is owed");
        assert_eq!(deferred.target, "thread-4", "the newest pick wins");
        assert_eq!(deferred.prompt.as_deref(), Some("hello"));
    }

    /// Ctrl+C takes back the prompt. The switch is a separate request and stands.
    #[test]
    fn interrupting_a_deferred_prompt_leaves_the_switch_owed() {
        let mut state = starting_state();
        let mut queued = None;
        hold_until_thread(
            &mut state,
            Action::ResumeThread("thread-9".to_owned()),
            &mut queued,
        );
        state.handle_paste("hello");
        let submit = state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        hold_until_thread(&mut state, submit, &mut queued);

        let interrupt = state.handle_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        hold_until_thread(&mut state, interrupt, &mut queued);

        assert!(!state.busy);
        let deferred = state
            .take_deferred_resume()
            .expect("the switch still stands");
        assert_eq!(deferred.target, "thread-9");
        assert_eq!(deferred.prompt, None, "the prompt was taken back");
    }

    /// `/new` wipes first and loads after: the old transcript is gone and the fresh
    /// welcome panel is up with the spinner, rather than the previous conversation
    /// sitting frozen on screen for the length of `thread/start`.
    #[test]
    fn new_thread_clears_the_screen_before_the_wait_begins() {
        let mut state = starting_state();
        state.attach_thread("thread-1".to_owned(), ".".to_owned(), "gpt-5.6-sol", None);
        state.handle_paste("old prompt");
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        state.prepare_new_thread();
        state.begin_thread_switch();

        assert!(
            state.thread_pending(),
            "the wait renders as a pending session"
        );
        assert!(!state.busy, "the previous turn does not carry over");
        assert!(state.editor.is_empty());
        assert!(state.drain_committed().is_empty(), "transcript was wiped");

        let view = state.view();
        assert!(
            view.welcome.is_some(),
            "the fresh welcome panel is already up"
        );
        assert!(view.status_line.is_some(), "the status line stays painted");
        assert_eq!(
            view.activity, None,
            "the wait is silent: the screen just reads as ready"
        );
    }

    /// `/resume` wipes the same way, minus the welcome card — the restored history
    /// is what fills the screen, so a panel that flashed and vanished would be noise.
    #[test]
    fn resume_clears_the_screen_before_the_wait_begins() {
        let mut state = starting_state();
        state.attach_thread("thread-1".to_owned(), ".".to_owned(), "gpt-5.6-sol", None);
        state.handle_paste("old prompt");
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        state.prepare_resume();
        state.begin_thread_switch();

        assert!(state.thread_pending());
        assert!(!state.busy);
        assert!(state.drain_committed().is_empty(), "transcript was wiped");

        let view = state.view();
        assert!(view.welcome.is_none());
        assert!(view.status_line.is_some(), "the status line stays painted");
        assert_eq!(view.activity, None, "the wait is silent");
    }

    /// A failed switch must not strand the UI pending against a thread that will
    /// never arrive — the session goes back to the one it left.
    #[test]
    fn a_failed_switch_falls_back_to_the_previous_thread() {
        let mut state = starting_state();
        state.attach_thread("thread-1".to_owned(), ".".to_owned(), "gpt-5.6-sol", None);
        let previous = state.thread_id.clone();

        state.prepare_resume();
        state.begin_thread_switch();
        assert!(state.thread_pending());

        abandon_thread_switch(&mut state, previous, "thread/resume 실패");

        assert!(!state.thread_pending());
        assert_eq!(state.thread_id, "thread-1");
        assert!(!state.busy);
        assert_eq!(state.view().activity, None, "the spinner stops");
    }

    #[test]
    fn a_resume_response_missing_its_thread_is_rejected() {
        assert!(parse_resumed_thread(&json!({ "cwd": ".", "model": "m" })).is_err());
        assert!(parse_resumed_thread(&json!({ "thread": { "id": "t" }, "model": "m" })).is_err());

        let resumed = parse_resumed_thread(&json!({
            "thread": { "id": "thread-9", "turns": [] },
            "cwd": "/repo",
            "model": "gpt-5.6-sol",
            "reasoningEffort": "xhigh"
        }))
        .expect("a complete response");

        assert_eq!(resumed.id, "thread-9");
        assert_eq!(resumed.cwd, "/repo");
        assert_eq!(resumed.model, "gpt-5.6-sol");
        assert_eq!(resumed.effort.as_deref(), Some("xhigh"));
    }

    /// Once the new session lands the loading state has to clear itself, or the
    /// spinner would outlive the wait.
    #[test]
    fn attaching_a_new_thread_clears_the_loading_state() {
        let mut state = starting_state();
        state.prepare_new_thread();
        state.begin_thread_switch();

        state.attach_thread("thread-2".to_owned(), ".".to_owned(), "gpt-5.6-sol", None);

        assert!(!state.thread_pending());
        assert_eq!(state.view().activity, None);
    }

    /// The session id is the only thing missing from the first frame, so binding it
    /// must not disturb what is already on screen.
    #[test]
    fn attaching_the_thread_keeps_the_queued_prompt_on_screen() {
        let mut state = starting_state();
        state.handle_paste("hello");
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        state.attach_thread(
            "thread-1".to_owned(),
            ".".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );

        assert!(!state.thread_pending());
        assert_eq!(state.thread_id, "thread-1");
        assert!(state.busy, "the queued prompt still has to start its turn");
    }

    /// The shapes here are the real `marketplace/upgrade` replies observed from
    /// codex 0.145: the scope always comes back in `selectedMarketplaces`, even
    /// when nothing needed refreshing.
    #[test]
    fn upgrade_results_report_the_scope_the_server_actually_used() {
        let untouched = json!({
            "selectedMarketplaces": ["example-marketplace"],
            "upgradedRoots": [],
            "errors": []
        });
        assert_eq!(
            format_upgrade_result(&untouched),
            "example-marketplace · 이미 최신 상태입니다."
        );

        let upgraded = json!({
            "selectedMarketplaces": ["a", "b"],
            "upgradedRoots": ["C:/a"],
            "errors": []
        });
        assert_eq!(
            format_upgrade_result(&upgraded),
            "a, b · 1개를 갱신했습니다."
        );

        let failed = json!({
            "selectedMarketplaces": ["a"],
            "upgradedRoots": [],
            "errors": [{ "message": "fetch failed" }]
        });
        assert_eq!(
            format_upgrade_result(&failed),
            "a · 0개 갱신 · 실패: fetch failed"
        );

        // A reply that names no scope must not claim one.
        assert_eq!(
            format_upgrade_result(&json!({})),
            "Git 마켓플레이스 · 이미 최신 상태입니다."
        );
    }

    #[test]
    fn reload_reports_stay_silent_unless_something_needs_attention() {
        // A clean reload prints only its title.
        let healthy = vec![McpServerInfo::probe("chrome", "unsupported", 3)];
        assert_eq!(format_reload_report(None, None, Some(&healthy)), "");
        assert_eq!(format_reload_report(None, None, None), "");

        // Anything left unusable is still named.
        let servers = vec![
            McpServerInfo::probe("chrome", "unsupported", 3),
            McpServerInfo::probe("github", "notLoggedIn", 0),
        ];
        assert_eq!(
            format_reload_report(None, None, Some(&servers)),
            "로그인 필요: github"
        );

        // A failed reconnect must not be reported as a successful one.
        let failed = format_reload_report(
            Some("skills 조회 실패".to_owned()),
            Some("reload 거부".to_owned()),
            None,
        );
        assert!(failed.contains("MCP 재연결 실패 · reload 거부"), "{failed}");
        assert!(
            failed.contains("일부 목록을 갱신하지 못했습니다"),
            "{failed}"
        );
    }

    #[test]
    fn marketplace_sources_split_a_trailing_ref_but_leave_urls_alone() {
        assert_eq!(split_marketplace_ref("owner/repo"), ("owner/repo", None));
        assert_eq!(
            split_marketplace_ref("owner/repo@main"),
            ("owner/repo", Some("main"))
        );
        // An SSH URL's `@` separates the user, and a scheme or a path never
        // carries a ref, so none of these may be split.
        assert_eq!(
            split_marketplace_ref("git@github.com:owner/repo"),
            ("git@github.com:owner/repo", None)
        );
        assert_eq!(
            split_marketplace_ref("https://github.com/owner/repo"),
            ("https://github.com/owner/repo", None)
        );
        assert_eq!(
            split_marketplace_ref("./local/marketplace"),
            ("./local/marketplace", None)
        );
        assert_eq!(
            split_marketplace_ref("C:\\repos\\marketplace"),
            ("C:\\repos\\marketplace", None)
        );
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

    #[test]
    fn mouse_requests_preserve_wheel_scroll_and_route_left_drags() {
        let at = |kind, modifiers| MouseEvent {
            kind,
            column: 4,
            row: 7,
            modifiers,
        };

        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE
            )),
            MouseRequest::SelectionStart(4, 7)
        );
        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::NONE
            )),
            MouseRequest::SelectionUpdate(4, 7)
        );
        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE
            )),
            MouseRequest::SelectionEnd(4, 7)
        );
        assert_eq!(
            mouse_request(&at(MouseEventKind::ScrollUp, KeyModifiers::NONE)),
            MouseRequest::Scroll(WHEEL_ROWS)
        );
        assert_eq!(
            mouse_request(&at(MouseEventKind::Moved, KeyModifiers::NONE)),
            MouseRequest::Hover(4, 7)
        );

        // Shift hands selection back to the terminal and cancels any app drag
        // that began before the modifier was pressed.
        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::SHIFT
            )),
            MouseRequest::CancelSelection
        );
        assert_eq!(
            mouse_request(&at(MouseEventKind::ScrollUp, KeyModifiers::SHIFT)),
            MouseRequest::Scroll(WHEEL_ROWS)
        );
        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::NONE
            )),
            MouseRequest::None
        );
    }

    #[test]
    fn mouse_move_routes_hover_coordinates() {
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 12,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(mouse_request(&event), MouseRequest::Hover(12, 4));
    }
}
