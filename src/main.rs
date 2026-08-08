mod app_server;
mod backend;
mod claude;
mod completion;
mod devezcode;
mod editor;
mod integrations;
mod open_code;
mod paste;
mod perf;
mod pricing;
mod provider;
mod renderer;
mod rollout;
mod selection;
mod state;
mod syntax;
mod theme;
mod update;

use std::{
    env, fs,
    future::Future,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use app_server::ServerEvent;
use backend::BackendServer;
use arboard::{Clipboard, ImageData};
use clap::Parser;
use completion::collect_workspace_entries;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use editor::Editor;
use futures_util::StreamExt;
use integrations::{McpServerInfo, PluginCatalog, PluginDetail, PluginInfo, PluginScope};
use paste::{
    BufferedText, BufferedTextTarget, ComposerInput, ComposerPasteBuffer, PasteBurst,
};
use provider::{ProviderAuthKind, ProviderAuthRequest};
use renderer::{BlockKind, Pick, RenderMode, Renderer, SelectionResult, TerminalSession, View};
use serde_json::{Value, json};
use state::{
    AccountPlan, Action, AppState, DiffDisplayMode, LoginMethod, ModelInfo, SessionInfo,
    SessionPicker, SessionPickerResult, ShellDisplayMode, VibeMode, load_model_context_windows,
};
use tokio::{sync::mpsc, time::MissedTickBehavior};

#[derive(Parser)]
#[command(
    name = "dvz",
    version,
    about = "Stable terminal UI for Codex and Claude Agent SDK"
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

    /// OpenCode executable used to launch `opencode acp`.
    #[arg(long, default_value = "opencode", hide = true)]
    open_code: PathBuf,

    /// Claude Code executable whose existing subscription login is reused.
    #[arg(long, default_value = "claude")]
    claude: PathBuf,

    /// Node.js executable used by the Claude Agent SDK bridge.
    #[arg(long, default_value = "node", hide = true)]
    node: PathBuf,

    /// UI theme: minimal, soft, dark, gray, softpink, or midnight.
    #[arg(long, value_name = "THEME")]
    theme: Option<String>,

    /// Renderer: fullscreen pins the composer and status line to the bottom and
    /// scrolls the transcript itself; inline hands the transcript to the
    /// terminal's own scrollback. Saved in %APPDATA%\DevezVibe\renderer.txt, or
    /// set DEVEZ_VIBE_RENDERER.
    #[arg(long, value_name = "RENDERER")]
    renderer: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Install the latest published release from npm.
    Update,
    /// Print the Devez Vibe version.
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command.as_ref() {
        match command {
            Command::Update => return update::run_self_update(),
            Command::Version => {
                println!("Devez Vibe v{}", update::CURRENT_VERSION);
                return Ok(());
            }
        }
    }
    let selected_theme = theme::load(cli.theme.as_deref())?;
    theme::set_current(selected_theme);
    devezcode::init();
    let cwd = resolve_cwd(cli.cwd.as_deref())?;
    let mut server = BackendServer::spawn(
        &cli.codex,
        &cli.open_code,
        &cli.node,
        &cli.claude,
        &cwd,
    )
    .await?;

    let result = run(&cli, &mut server).await;
    server.shutdown().await;
    devezcode::finish();
    result
}

async fn run(cli: &Cli, server: &mut BackendServer) -> Result<()> {
    server.initialize().await?;
    let provider_config = backend::read_provider_config();
    let startup_config = read_startup_config();
    let requested_model = requested_startup_model(cli.model.as_deref(), &provider_config);
    let requested_codex = requested_model.is_some_and(is_codex_model);
    // Codex has to be up before the session and model lists are read: both skip their
    // Codex half while the app-server is down, so `dvz -r <gpt thread>` would resolve
    // against a Claude-only world — no GPT models to show, no GPT sessions to pick.
    let resumes_codex = cli.continue_session
        || cli
            .resume
            .as_deref()
            // An empty `--resume` opens the startup picker, which has to offer both
            // halves of the session list.
            .is_some_and(|target| target.is_empty() || server.thread_is_codex(target));
    if requested_codex || resumes_codex {
        let _ = server.start_codex().await;
    }
    let codex_unavailable_reason = server.codex_unavailable_reason().map(ToOwned::to_owned);
    let cwd = resolve_cwd(cli.cwd.as_deref())?;
    let resume_id = resolve_startup_session(cli, server, &cwd).await?;
    let Some(resume_id) = resume_id else {
        return Ok(());
    };
    let is_resuming = !resume_id.is_empty();
    // The thread being resumed owns the runtime it was recorded under. Only a launch
    // that starts a new session falls back to the Claude-first default.
    let resumed_codex = is_resuming && server.thread_is_codex(&resume_id) && server.has_codex();
    let default_to_claude = requested_model.is_none() && !resumed_codex;
    let requested_claude = requested_model.is_some_and(claude::is_claude_model)
        || (is_resuming && claude::is_claude_thread(&resume_id));
    let requested_open_code = requested_model.is_some_and(open_code::is_open_code_model);
    let fallback_to_claude = should_fallback_to_claude(server.has_codex(), requested_model);
    let (account, prefer_open_code) = if requested_claude
        || default_to_claude
        || fallback_to_claude
    {
        ("Claude subscription".to_owned(), false)
    } else if requested_open_code {
        ("OpenCode".to_owned(), false)
    } else {
        match ensure_account(server).await {
            Ok(account) => (account, false),
            Err(_) if server.has_open_code() => ("OpenCode · /connect".to_owned(), true),
            // A thread that already exists still has to open; only the header label
            // is unknown when the account probe fails.
            Err(_) if is_resuming => ("Codex".to_owned(), false),
            Err(error) => return Err(error),
        }
    };

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
    let fallback_open_code = prefer_open_code
        .then(|| {
            models
                .iter()
                .find(|model| open_code::is_open_code_model(&model.model))
                .map(|model| model.model.as_str())
        })
        .flatten();
    let preferred_claude = (default_to_claude || fallback_to_claude)
        .then(|| preferred_claude_model(&models))
        .flatten();
    let startup_model_request =
        preferred_claude.or_else(|| cli.model.as_deref().or(fallback_open_code));

    let startup_model = resolve_startup_model(
        &models,
        startup_model_request,
        cli.effort.as_deref(),
        &startup_config,
    )?;
    let model_override = if is_resuming {
        cli.model.clone()
    } else {
        Some(startup_model.model.clone())
    };

    // Everything the first frame shows — plan, cwd, account, model, limits, branch —
    // is already known here. Only the session id arrives late, so the state is built
    // now and the screen goes up before the slow `thread/start` round trip.
    let mut state = AppState::new(
        String::new(),
        cwd.to_string_lossy().into_owned(),
        account,
        models,
        &startup_model.model,
        Some(&startup_model.effort),
    );
    if fallback_to_claude {
        state.push_notice(
            BlockKind::Warning,
            "Codex 사용 불가",
            format!(
                "{}\nClaude provider로 자동 전환했습니다.",
                codex_unavailable_reason
                    .as_deref()
                    .unwrap_or("Codex app-server가 종료되었습니다.")
            ),
        );
    }
    // No runtime is connected until this machine has picked one, so a fresh
    // install opens the picker on the first frame rather than assuming.
    state.prompt_for_provider_if_unconnected();

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
        &startup_model.model,
        &startup_model.effort,
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
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    cli: &Cli,
    cwd: &Path,
    resume_id: &str,
    is_resuming: bool,
    model_override: Option<&str>,
    requested_model_name: &str,
    requested_effort: &str,
) -> Result<()> {
    state.set_host_loading(is_resuming);
    if is_resuming {
        server.prepare_resume_runtime(resume_id).await?;
    }
    let claude = claude_session_settings(state);
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
            state.model_verbosity(),
            &claude,
            cli.effort.as_deref(),
            requested_effort,
        ),
        read_runtime_account_plan(server, requested_model_name),
        None,
    )
    .await?;
    let Startup::Ready {
        thread_response,
        queued,
    } = startup
    else {
        return Ok(());
    };
    apply_claude_account_metadata(state, &thread_response);

    let thread = if is_resuming {
        hydrate_thread_history(server, &thread_response).await?
    } else {
        thread_with_initial_turns(&thread_response)?
    };
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
    let actual_effort = thread_response
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| is_resuming.then(|| cli.effort.clone()).flatten())
        .unwrap_or_else(|| requested_effort.to_owned());
    validate_effort(state.models(), &actual_model, Some(&actual_effort))?;
    let actual_cwd = thread_response
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| cwd.to_str().unwrap_or("."))
        .to_owned();

    let rollout = is_resuming
        .then(|| state::codex_home().and_then(|home| rollout::load(&home, &thread_id)))
        .flatten();
    state.attach_thread(thread_id, actual_cwd, &actual_model, Some(&actual_effort));
    // A thread whose turns already moved to another runtime resumes from that
    // runtime's session, not from the id it is named after. The routing knows; the
    // thread id does not.
    state.note_resume_id(&server.resume_id(&state.thread_id));
    apply_deferred_startup_actions(server, state).await;
    if is_resuming {
        state.load_history(&thread, rollout.as_ref());
        state.begin_cost_restore();
        apply_resumed_token_usage(state, &thread_response);
    }
    state.set_host_loading(false);
    draw(state, renderer)?;

    let (update_tx, update_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(latest) = update::check_for_update().await {
            let _ = update_tx.send(latest).await;
        }
    });

    if let Some(text) = queued {
        draw(state, renderer)?;
        start_turn(server, state, text, None).await;
    }
    event_loop(server, state, renderer, update_rx).await
}

/// Runs the full UI while `thread/start` is still in flight: the screen is live,
/// the composer accepts typing, and the account plan drops in as soon as it lands.
/// Used both at launch and by `/new`, so the wait always looks the same.
async fn await_thread(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread: impl Future<Output = Result<Value>>,
    plan: impl Future<Output = AccountPlan>,
    mut side_exit_key_guard: Option<Instant>,
) -> Result<Startup> {
    let mut events = EventStream::new();
    let mut composer_paste = ComposerPasteBuffer::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(120));
    spinner_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut queued = None;
    let mut plan_pending = true;
    let mut redraw = true;
    tokio::pin!(thread);
    tokio::pin!(plan);

    loop {
        if redraw {
            draw(state, renderer)?;
        }
        let paste_deadline = composer_paste.flush_deadline();
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
                        if suppress_side_exit_key(
                            &mut side_exit_key_guard,
                            &key,
                            Instant::now(),
                        ) {
                            state.disarm_quit();
                            Action::None
                        } else if is_clipboard_image_shortcut(&key) && attach_clipboard_image(state) {
                            Action::None
                        } else if expand_collapsed_paste_shortcut(
                            state,
                            &mut composer_paste,
                            &key,
                            Instant::now(),
                        ) {
                            Action::Tick(true)
                        } else {
                            observe_composer_key(state, &mut composer_paste, key, Instant::now())
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => renderer_mouse_action(renderer, &mouse, |pick| pick_action(state, pick)),
                    Some(Ok(Event::Paste(text))) => {
                        renderer.clear_selection();
                        if composer_paste.take_discarded_paste(&text) {
                            // The shortcut already expanded the block this
                            // payload stands for.
                            Action::Tick(true)
                        } else {
                        flush_composer_paste(state, &mut composer_paste, Instant::now());
                        if let Some(action) = state.paste_as_prompt_answer(&text) {
                            action
                        } else {
                            if !attach_clipboard_image(state) {
                                apply_direct_paste(state, &text);
                            }
                            Action::None
                        }
                        }
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
            _ = wait_for_paste_flush(paste_deadline), if paste_deadline.is_some() => {
                Action::Tick(flush_composer_paste(state, &mut composer_paste, Instant::now()))
            }
            _ = spinner_tick.tick() => Action::Tick(state.tick()),
        };

        redraw = !matches!(&action, Action::Tick(false)) && !composer_paste.is_buffering();
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
        Action::ScrollToBottom => Some(Action::ScrollToBottom),
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
        // These controls already update their in-memory value when clicked, and
        // the queued first prompt reads that value. Delay only the RPC persistence
        // until the thread id arrives instead of discarding the click.
        Action::SetFast(enabled) => {
            state.set_fast_mode(enabled);
            state.defer_startup_action(Action::SetFast(enabled));
            None
        }
        action @ (Action::SetClaudePermissionMode(_)
        | Action::PersistVibeDisplayModes { .. }) => {
            state.defer_startup_action(action);
            None
        }
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

/// Fills the resume picker from `thread/list`. Shared so the picker looks the same
/// whether it is opened from a live session or from a session still starting up.
async fn open_resume_picker(server: &BackendServer, state: &mut AppState) {
    match list_sessions(server, None, None, 100).await {
        Ok(sessions) => state.open_session_picker(sessions),
        Err(error) => state.push_notice(BlockKind::Error, "세션 목록 실패", error.to_string()),
    }
}

async fn resolve_startup_session(
    cli: &Cli,
    server: &BackendServer,
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
                plan_summary: None,
                plan_active: false,
                plan_shimmer_phase: None,
                editor: &editor,
                composer_images: &[],
                queued_prompts: Vec::new(),
                subagents: Vec::new(),
                composer_placeholder: "",
                welcome: None,
                suggestions: Vec::new(),
                activity: None,
                activity_model: None,
                activity_phase: 0.0,
                footer: "Resume a Codex session".to_owned(),
                status_line: None,
                composer_notice: composer_notice.clone(),
                composer_mode: None,
                chat_layout: false,
                shell_display_mode: ShellDisplayMode::Collapse,
                diff_display_mode: DiffDisplayMode::Collapse,
                side_panel_open: false,
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
                            Ok(()) => "• Copied to clipboard".to_owned(),
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
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    update_rx: mpsc::Receiver<String>,
) -> Result<()> {
    let mut update_rx = Some(update_rx);
    let mut terminal_events = EventStream::new();
    let mut composer_paste = ComposerPasteBuffer::new();
    let mut activity_tick = tokio::time::interval(Duration::from_millis(80));
    activity_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut resize = ResizeTracker::new();
    let (workspace_tx, mut workspace_rx) = mpsc::channel(1);
    let mut cost_restore_rx = None;
    let mut indexed_cwd = None;
    let mut integration_key = None;
    let mut integration_rx = None;
    let mut side_exit_key_guard = None;
    draw(state, renderer)?;

    loop {
        if let Some(thread_id) = state.take_cost_restore() {
            cost_restore_rx = Some(start_cost_restore(thread_id));
        }
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
        let current_integration_key = (state.thread_id.clone(), state.cwd.clone());
        if integration_key.as_ref() != Some(&current_integration_key) {
            integration_key = Some(current_integration_key);
            integration_rx = Some(start_integration_refresh(server, state));
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
        let mut animation_tick = false;
        let paste_deadline = composer_paste.flush_deadline();
        let action = tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        if suppress_side_exit_key(
                            &mut side_exit_key_guard,
                            &key,
                            Instant::now(),
                        ) {
                            state.disarm_quit();
                            renderer.clear_selection();
                            Action::None
                        } else if is_clipboard_image_shortcut(&key) && attach_clipboard_image(state) {
                            renderer.clear_selection();
                            Action::None
                        } else if expand_collapsed_paste_shortcut(
                            state,
                            &mut composer_paste,
                            &key,
                            Instant::now(),
                        ) {
                            renderer.clear_selection();
                            Action::Tick(true)
                        } else if is_selection_delete_key(&key)
                            && let Some(range) = renderer.composer_selection_range()
                            && state.delete_composer_selection(range)
                        {
                            // The drag selected composer text, so the key takes the
                            // selection rather than the character at the cursor.
                            renderer.clear_selection();
                            Action::Tick(true)
                        } else if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            if let Some(text) = renderer.selected_text() {
                                // This Ctrl+C is a copy, so it neither arms nor
                                // spends the quit.
                                state.disarm_quit();
                                renderer.clear_selection();
                                Action::Copy(text)
                            } else {
                                // Typing means the drag is over and its highlight is
                                // stale, so it goes before the key is acted on.
                                let cleared = renderer.clear_selection();
                                let action = observe_composer_key_with_scroll(
                                    state,
                                    renderer,
                                    &mut composer_paste,
                                    key,
                                    Instant::now(),
                                );
                                match action {
                                    Action::Tick(false) if cleared => Action::Tick(true),
                                    action => action,
                                }
                            }
                        } else {
                        // Typing means the drag is over and its highlight is
                        // stale, so it goes before the key is acted on.
                        let cleared = renderer.clear_selection();
                        let action = observe_composer_key_with_scroll(
                            state,
                            renderer,
                            &mut composer_paste,
                            key,
                            Instant::now(),
                        );
                        match action {
                            Action::Tick(false) if cleared => Action::Tick(true),
                            action => action,
                        }
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        // Clicking or scrolling is input as well, so a Ctrl+C armed
                        // before it must not be spent by the Ctrl+C after it.
                        state.disarm_quit();
                        renderer_mouse_action(renderer, &mouse, |pick| pick_action(state, pick))
                    }
                    Some(Ok(Event::Paste(text))) => {
                        renderer.clear_selection();
                        if composer_paste.take_discarded_paste(&text) {
                            // The shortcut already expanded the block this
                            // payload stands for.
                            Action::Tick(true)
                        } else {
                        flush_composer_paste(state, &mut composer_paste, Instant::now());
                        if let Some(action) = state.paste_as_prompt_answer(&text) {
                            action
                        } else {
                            if !attach_clipboard_image(state) {
                                apply_direct_paste(state, &text);
                            }
                            Action::None
                        }
                        }
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
                        // Claude-only hand-off: this notification carries the SDK's
                        // fresh account usage at the end of every turn, and the host
                        // has no statusLine hook for us to ride on. Codex limits
                        // arrive through `account/rateLimits/read` on another schema
                        // and never reach here.
                        if method == "claude/account/updated" {
                            devezcode::publish_claude_rate_limits(
                                params.get("usage").filter(|value| !value.is_null()),
                            );
                        }
                        let interrupt_after_start = method == "turn/started"
                            && state.take_pending_interrupt().is_some();
                        if state.take_account_refresh() {
                            refresh_account(server, state).await;
                        }
                        if interrupt_after_start {
                            Action::Interrupt
                        } else if method == "turn/completed"
                            // A runtime that compacts without running a turn ends
                            // the wait here, so the queue drains from here too.
                            || (method == "thread/compacted" && !state.host_turn_busy())
                        {
                            state
                                .take_queued_prompt()
                                .map(|text| state.start_queued_prompt(text))
                                .unwrap_or(Action::None)
                        } else if method == "skills/changed" {
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
                    Some(ServerEvent::ProviderUnavailable { provider, message }) => {
                        if provider == "Codex" {
                            state.fallback_from_codex(message);
                        } else {
                            state.push_notice(
                                BlockKind::Warning,
                                format!("{provider} 사용 불가"),
                                message,
                            );
                        }
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
            Some(restored) = recv_cost_restore(&mut cost_restore_rx) => {
                state.apply_restored_cost(&restored.thread_id, restored.ledger);
                Action::None
            }
            Some(catalog) = recv_integrations(&mut integration_rx) => {
                if let Err(error) = apply_integrations(state, catalog) {
                    state.push_notice(BlockKind::Warning, "통합 기능 조회 실패", error.to_string());
                }
                Action::None
            }
            Some((cwd, entries)) = workspace_rx.recv() => {
                if state.cwd == cwd {
                    state.update_workspace_entries(entries);
                }
                Action::None
            }
            _ = wait_for_paste_flush(paste_deadline), if paste_deadline.is_some() => {
                Action::Tick(flush_composer_paste(state, &mut composer_paste, Instant::now()))
            }
            _ = activity_tick.tick() => {
                let tick = state.render_tick();
                let mut redraw = tick.redraw;
                animation_tick = tick.animation_only;
                // Ctrl+wheel font zoom changes the cell grid without always
                // sending a `Resize`, so the size is polled here as well.
                resize.observe(terminal_size());
                if resize.settled() {
                    renderer.relayout()?;
                    redraw = true;
                    animation_tick = false;
                } else if resize.pending() {
                    // Nothing painted onto a grid that is still moving survives.
                    redraw = false;
                }
                Action::Tick(redraw)
            }
        };

        // Windows exposes a paste as many key events. Do not render between
        // those events while the composer is collecting them; rendering is
        // much slower than parsing and used to make long pastes crawl.
        let redraw = !matches!(&action, Action::Tick(false)) && !composer_paste.is_buffering();
        let returning_from_side = matches!(&action, Action::ReturnFromSide);
        let should_quit = execute_action(server, state, renderer, action).await?;
        if returning_from_side {
            side_exit_key_guard = Some(Instant::now() + SIDE_EXIT_KEY_SETTLE);
        }
        if redraw {
            let animation_started = Instant::now();
            let animated =
                animation_tick && renderer.render_animation(state.animation_view())?;
            if animated {
                perf::record_animation(animation_started.elapsed());
            }
            if !animated {
                draw(state, renderer)?;
            }
        }
        if should_quit || connection_closed {
            break;
        }
    }
    Ok(())
}

const WHEEL_ROWS: isize = 3;
const SIDE_EXIT_KEY_SETTLE: Duration = Duration::from_millis(250);

fn is_side_exit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Key-repeat records can remain queued while the parent thread is being
/// resumed. Keep extending the guard while they arrive so a held close key can
/// never become an interrupt or quit on the parent screen.
fn suppress_side_exit_key(
    guard: &mut Option<Instant>,
    key: &KeyEvent,
    now: Instant,
) -> bool {
    let Some(until) = *guard else {
        return false;
    };
    if now >= until {
        *guard = None;
        return false;
    }
    if !is_side_exit_key(key) {
        return false;
    }
    *guard = Some(now + SIDE_EXIT_KEY_SETTLE);
    true
}

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
    let request = mouse_request(mouse);
    // Some embedded terminals deliver the press but swallow the matching
    // release.  Chrome controls must not depend on that release: activate a
    // known pick as soon as it is pressed, while plain text keeps the normal
    // drag-to-select path below.
    if let MouseRequest::SelectionStart(column, row) = request {
        if let Some(pick) = renderer.pick_at(column, row) {
            let cleared = renderer.clear_selection();
            return match on_pick(pick) {
                Action::Tick(changed) => Action::Tick(changed || cleared),
                action => action,
            };
        }
    }

    match request {
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
                if let Some(text) = renderer.double_click_word(column, row) {
                    return Action::Copy(text);
                }
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
        Pick::VibeMode => {
            let (shell, diff) = state.cycle_vibe_mode();
            Action::PersistVibeDisplayModes { vibe: state.vibe_mode(), response: state.response_length(), shell, diff }
        }
        Pick::ResponseLength => {
            state.cycle_response_length();
            Action::PersistVibeDisplayModes {
                vibe: state.vibe_mode(), response: state.response_length(),
                shell: state.shell_display_mode(), diff: state.diff_display_mode(),
            }
        }
        Pick::ShellDisplayMode => {
            state.cycle_shell_display_mode();
            Action::PersistVibeDisplayModes { vibe: state.vibe_mode(), response: state.response_length(), shell: state.shell_display_mode(), diff: state.diff_display_mode() }
        }
        Pick::DiffDisplayMode => {
            state.cycle_diff_display_mode();
            Action::PersistVibeDisplayModes { vibe: state.vibe_mode(), response: state.response_length(), shell: state.shell_display_mode(), diff: state.diff_display_mode() }
        }
        Pick::PlanSummary => {
            state.toggle_plan_summary();
            Action::Tick(true)
        }
        Pick::RemoveQueuedPrompt(index) => {
            state.remove_queued_prompt(index);
            Action::Tick(true)
        }
        Pick::OpenLink(target) => Action::OpenUrl(target),
        Pick::FastMode => Action::SetFast(!state.effective_fast_mode()),
        Pick::ClaudePermissionMode => {
            Action::SetClaudePermissionMode(state.cycle_claude_permission_mode())
        }
        Pick::Model => state.run_command("/model"),
        Pick::EffortSetting => state.run_command("/effort"),
        Pick::Subagent(index) => state.open_subagent(index),
        Pick::ScrollToBottom => Action::ScrollToBottom,
        Pick::Close => {
            state.close_overlay()
        }
        Pick::Row(index) => state.click_overlay_row(index),
        Pick::Effort(step) => state.click_effort_step(step),
    }
}

/// Maps a key to a transcript scroll, or `None` to let the session have it.
/// PageDown always returns a fullscreen transcript to its latest row. Shift
/// keeps PageUp out of the way of composer and picker cursor navigation.
fn scroll_request(renderer: &Renderer, key: &KeyEvent) -> Option<isize> {
    if renderer.mode() != RenderMode::Fullscreen {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down {
        return Some(isize::MIN);
    }
    if key.code == KeyCode::PageDown {
        return Some(isize::MIN);
    }
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    match key.code {
        KeyCode::PageUp => Some(renderer.page_rows()),
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
    server: &mut BackendServer,
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
        | Action::ScrollToBottom
        | Action::Quit) => return execute_local_action(state, renderer, action),
        Action::Submit(text) => {
            renderer.scroll_to_bottom();
            let handoff = provider_handoff_snapshot(state, renderer);
            start_turn(server, state, text, Some(handoff)).await
        }
        Action::Steer(text) => {
            renderer.scroll_to_bottom();
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
        Action::ActivateCodex => activate_codex(server, state).await,
        Action::PersistProviderConnection {
            key_path,
            connected,
            activate_codex,
        } => {
            match server
                .request(
                    "config/value/write",
                    config_value_write_params(key_path, &connected.to_string()),
                )
                .await
            {
                Ok(_) => {
                    if activate_codex {
                        crate::activate_codex(server, state).await;
                    }
                }
                Err(error) => {
                    // The switch never reached disk, so the row goes back to what
                    // the next launch will actually read.
                    state.restore_provider_connection(key_path, !connected);
                    state.push_notice(
                        BlockKind::Warning,
                        "Provider 연결 저장 실패",
                        error.to_string(),
                    );
                }
            }
        }
        Action::SetFast(enabled) => {
            set_fast_mode(server, state, enabled).await;
        }
        // The mode also rides along with every turn, so a session that has not
        // started yet still opens under it. This call is what moves a live one.
        Action::SetClaudePermissionMode(mode) => {
            set_claude_permission_mode(server, state, mode).await;
        }
        Action::PersistShellDisplayMode(mode) => {
            if let Err(error) = server
                .request(
                    "config/value/write",
                    config_value_write_params("shell_display_mode", mode.config_value()),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Warning,
                    "Shell 표시 설정 저장 실패",
                    error.to_string(),
                );
            }
        }
        Action::PersistDiffDisplayMode(mode) => {
            if let Err(error) = server
                .request(
                    "config/value/write",
                    config_value_write_params("diff_display_mode", mode.config_value()),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Warning,
                    "Diff 표시 설정 저장 실패",
                    error.to_string(),
                );
            }
        }
        Action::PersistVibeDisplayModes { vibe, response, shell, diff } => {
            persist_vibe_display_modes(server, state, vibe, response, shell, diff).await;
        }
        Action::PersistStatusLine { key_path, enabled } => {
            if let Err(error) = server
                .request(
                    "config/value/write",
                    config_value_write_params(key_path, &enabled.to_string()),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Warning,
                    "상태줄 표시 설정 저장 실패",
                    error.to_string(),
                );
            }
        }
        Action::StartSide(prompt) => {
            let response = server
                .request(
                    "thread/fork",
                    json!({
                        "threadId": state.thread_id,
                        "model": state.selected_model_name(),
                        "effort": state.selected_effort(),
                        "claudeDeveloperInstructions": CLAUDE_DEVEZ_INSTRUCTIONS,
                        "serviceTier": state.service_tier(),
                        "ephemeral": true,
                        "threadSource": "devez-vibe"
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
                            start_turn(server, state, prompt, None).await;
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
            let child_turn = state.turn_id.clone();
            let parent_thread = state.side_parent_thread_id().map(ToOwned::to_owned);
            // `thread/resume` rebuilds the view from stored history, which knows
            // nothing about a turn still in flight. Carry it across by hand.
            let parent_turn = state.take_side_parent_turn();
            if let Some(parent_thread) = parent_thread {
                // Only the ephemeral child IDs are used here; the parent turn
                // remains untouched while its screen is restored.
                if let Some(turn_id) = child_turn {
                    let _ = server
                        .request(
                            "turn/interrupt",
                            json!({ "threadId": child_thread, "turnId": turn_id }),
                        )
                        .await;
                }
                match resume_into_state(server, state, renderer, &parent_thread, true).await? {
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
                    // `resume_into_state` already posted the failure. Preserve
                    // the marker so retrying Esc/Ctrl+C stays on this safe path.
                    Switched::Failed => {
                        state.restore_side_parent(parent_thread, parent_turn);
                    }
                }
            }
        }
        Action::Compact => {
            // The runtime reports no assistant output while it compacts, so the
            // activity row owns the wait: spinner in, `Context compacted` out. The
            // clock starts before the request, since a runtime that only answers
            // once compaction finished would otherwise show no progress at all.
            state.begin_compaction();
            if let Err(error) = server
                .request(
                    "thread/compact/start",
                    json!({ "threadId": state.thread_id }),
                )
                .await
            {
                state.end_compaction();
                state.push_notice(BlockKind::Error, "압축 실패", error.to_string());
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
        Action::ConnectProvider => {
            state.open_provider_loading();
            draw(state, renderer)?;
            match server.provider_catalog().await {
                Ok(catalog) => state.open_provider_picker(&catalog),
                Err(error) => state.provider_connection_failed(error.to_string()),
            }
        }
        Action::SubmitProviderAuth(request) => {
            let ProviderAuthRequest {
                provider_id,
                provider_name,
                method_index,
                kind,
                inputs,
                api_key,
            } = *request;
            match kind {
                ProviderAuthKind::Api => {
                    let Some(api_key) = api_key else {
                        state.push_notice(
                            BlockKind::Error,
                            "Provider 연결 실패",
                            "API key가 없습니다.",
                        );
                        return Ok(false);
                    };
                    match server
                        .set_provider_api_key(&provider_id, &api_key, &inputs)
                        .await
                    {
                        Ok(()) => {
                            refresh_provider_models(server, state, &provider_name).await;
                        }
                        Err(error) => state.push_notice(
                            BlockKind::Error,
                            "Provider 연결 실패",
                            error.to_string(),
                        ),
                    }
                }
                ProviderAuthKind::OAuth => {
                    match server
                        .authorize_provider_oauth(&provider_id, method_index, &inputs)
                        .await
                    {
                        Ok(authorization) => {
                            let url = authorization
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let callback_method = authorization
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or("auto")
                                .to_owned();
                            let instructions = authorization
                                .get("instructions")
                                .and_then(Value::as_str)
                                .unwrap_or("브라우저에서 인증을 완료하세요.")
                                .to_owned();
                            state.open_provider_oauth(
                                provider_id.clone(),
                                provider_name.clone(),
                                method_index,
                                url.clone(),
                                instructions,
                                &callback_method,
                            );
                            if !url.is_empty()
                                && let Err(error) = open_url(&url)
                            {
                                state.push_notice(
                                    BlockKind::Warning,
                                    "브라우저 열기 실패",
                                    error.to_string(),
                                );
                            }
                            draw(state, renderer)?;
                            if callback_method == "auto" {
                                match server
                                    .complete_provider_oauth(
                                        &provider_id,
                                        method_index,
                                        None,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        refresh_provider_models(
                                            server,
                                            state,
                                            &provider_name,
                                        )
                                        .await;
                                    }
                                    Err(error) => {
                                        state.provider_connection_failed(error.to_string())
                                    }
                                }
                            }
                        }
                        Err(error) => state.push_notice(
                            BlockKind::Error,
                            "OAuth 시작 실패",
                            error.to_string(),
                        ),
                    }
                }
            }
        }
        Action::CompleteProviderOAuth {
            provider_id,
            provider_name,
            method,
            code,
        } => {
            match server
                .complete_provider_oauth(&provider_id, method, Some(&code))
                .await
            {
                Ok(()) => refresh_provider_models(server, state, &provider_name).await,
                Err(error) => {
                    state.push_notice(BlockKind::Error, "OAuth 연결 실패", error.to_string())
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
                    .request("config/value/write", config_value_write_params(key, &value))
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
/// Brings the Codex app-server up and moves the session onto it. Reached from
/// `/provider`, both when Codex is already connected and right after the pick
/// that connected it.
async fn activate_codex(server: &mut BackendServer, state: &mut AppState) {
    match server.start_codex().await {
        Ok(()) => match server
            .request("model/list", json!({ "includeHidden": false, "limit": 100 }))
            .await
        {
            Ok(response) => {
                state.replace_models(parse_models(&response));
                state.switch_to_codex();
            }
            Err(error) => {
                state.push_notice(BlockKind::Error, "Codex 모델 조회 실패", error.to_string())
            }
        },
        Err(error) => state.push_notice(BlockKind::Error, "Codex 사용 불가", error.to_string()),
    }
}

/// welcome panel goes up immediately, then `thread/start` is waited out behind a
/// live screen exactly the way launch does it. Returns `true` when the user quits
/// during the wait.
async fn start_new_thread(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<bool> {
    // Read the request out of the old session before it is torn down.
    let selected_model = state.selected_model_name().to_owned();
    let params = new_thread_params(
        &state.cwd,
        Some(&selected_model),
        Some(state.service_tier()),
        "clear",
        state.model_verbosity(),
        state.claude_permission_mode_setting().wire(),
        state.selected_effort(),
    );
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
        None,
    )
    .await?
    {
        Switch::Ready { response, queued } => (response, queued),
        Switch::Quit => return Ok(true),
        Switch::Failed => return Ok(false),
    };
    apply_claude_account_metadata(state, &response);

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
    state.note_resume_id(&server.resume_id(&state.thread_id));
    finish_thread_switch(server, state, renderer, queued).await
}

/// `/resume`, given the same treatment as [`start_new_thread`]: the screen resets
/// to a loading state straight away and the restored transcript arrives when
/// `thread/resume` answers. Returns `true` when the user quits during the wait.
async fn resume_thread(
    server: &mut BackendServer,
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

    if let Err(error) = server.prepare_resume_runtime(&thread_id).await {
        state.push_notice(BlockKind::Error, "세션 재개 실패", error.to_string());
        return Ok(false);
    }

    match resume_into_state(server, state, renderer, &thread_id, false).await? {
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
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    thread_id: &str,
    protect_side_exit_keys: bool,
) -> Result<Switched> {
    let previous_thread = state.thread_id.clone();
    // Read before the screen resets: the resumed session has to reopen on the
    // model, effort and permission mode the picker currently holds.
    let claude = claude_session_settings(state);
    renderer.clear_screen()?;
    state.prepare_resume();
    state.begin_thread_switch();
    state.set_host_loading(true);

    let (response, queued) = match await_switch(
        server,
        state,
        renderer,
        previous_thread.clone(),
        server.request("thread/resume", resume_thread_params(thread_id, &claude)),
        protect_side_exit_keys.then(|| Instant::now() + SIDE_EXIT_KEY_SETTLE),
    )
    .await?
    {
        Switch::Ready { response, queued } => (response, queued),
        Switch::Quit => return Ok(Switched::Quit),
        Switch::Failed => return Ok(Switched::Failed),
    };
    apply_claude_account_metadata(state, &response);

    let resumed = match parse_resumed_thread(&response) {
        Ok(resumed) => resumed,
        Err(error) => {
            abandon_thread_switch(state, previous_thread, error.to_string());
            return Ok(Switched::Failed);
        }
    };
    let rollout_id = server
        .active_codex_thread_id(&resumed.id)
        .unwrap_or_else(|| resumed.id.clone());
    let rollout = state::codex_home().and_then(|home| rollout::load(&home, &rollout_id));
    state.attach_thread(
        resumed.id,
        resumed.cwd,
        &resumed.model,
        resumed.effort.as_deref(),
    );
    state.note_resume_id(&server.resume_id(&state.thread_id));
    let history = match hydrate_thread_history(server, &response).await {
        Ok(history) => history,
        Err(error) => {
            abandon_thread_switch(state, previous_thread, error.to_string());
            return Ok(Switched::Failed);
        }
    };
    state.load_history(&history, rollout.as_ref());
    state.begin_cost_restore();
    apply_resumed_token_usage(state, &response);
    state.set_host_loading(false);
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
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    previous_thread: String,
    request: impl Future<Output = Result<Value>>,
    side_exit_key_guard: Option<Instant>,
) -> Result<Switch> {
    let model = state.selected_model_name().to_owned();
    match await_thread(
        server,
        state,
        renderer,
        request,
        read_runtime_account_plan(server, &model),
        side_exit_key_guard,
    )
    .await
    {
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
    state.set_host_loading(false);
    state.cancel_thread_switch(previous_thread_id);
    state.set_request_failed(message);
}

/// Tail shared by `/new` and `/resume`: paint the bound session, reload the
/// catalogues, then send whatever was typed during the wait.
async fn finish_thread_switch(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    queued: Option<String>,
) -> Result<bool> {
    apply_deferred_startup_actions(server, state).await;
    draw(state, renderer)?;
    if let Some(text) = queued {
        draw(state, renderer)?;
        send_queued_prompt(server, state, text).await;
    }
    Ok(false)
}

/// Commits mode clicks made while a new session had no id yet. Their local value
/// was already visible and will be used by any queued first prompt; this only
/// synchronizes the new thread and the saved default once both are addressable.
async fn apply_deferred_startup_actions(server: &BackendServer, state: &mut AppState) {
    for action in state.take_deferred_startup_actions() {
        match action {
            Action::SetFast(enabled) => set_fast_mode(server, state, enabled).await,
            Action::SetClaudePermissionMode(mode) => {
                set_claude_permission_mode(server, state, mode).await
            }
            Action::PersistVibeDisplayModes { vibe, response, shell, diff } => {
                persist_vibe_display_modes(server, state, vibe, response, shell, diff).await
            }
            _ => unreachable!("only startup-safe mode actions are deferred"),
        }
    }
}

/// Sends a prompt typed during a switch. Returning from a side conversation can
/// bring a turn back with it, so the prompt joins that turn rather than starting a
/// competing one.
async fn send_queued_prompt(server: &BackendServer, state: &mut AppState, text: String) {
    let Some(turn_id) = state.turn_id.clone() else {
        start_turn(server, state, text, None).await;
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
    id: String,
    cwd: String,
    model: String,
    effort: Option<String>,
}

fn parse_resumed_thread(response: &Value) -> Result<ResumedThread> {
    let thread = thread_with_initial_turns(response)?;
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
        id,
        cwd,
        model,
        effort,
    })
}

/// `thread/resume` returns requested turns in a top-level page. Normalize that
/// page back into the `thread.turns` shape consumed by `AppState::load_history`.
fn thread_with_initial_turns(response: &Value) -> Result<Value> {
    let mut thread = response
        .get("thread")
        .context("thread/resume 응답에 thread가 없습니다.")?
        .clone();
    if let Some(turns) = response.pointer("/initialTurnsPage/data").cloned() {
        thread
            .as_object_mut()
            .context("thread/resume 응답의 thread 형식이 올바르지 않습니다.")?
            .insert("turns".to_owned(), turns);
    } else if !thread.get("turns").is_some_and(Value::is_array) {
        thread
            .as_object_mut()
            .context("thread/resume 응답의 thread 형식이 올바르지 않습니다.")?
            .insert("turns".to_owned(), json!([]));
    }
    Ok(thread)
}

/// Hydrates every turn page before rendering. The resume response is only a
/// bootstrap: list from the first page again because it may omit `nextCursor`.
async fn hydrate_thread_history(server: &BackendServer, response: &Value) -> Result<Value> {
    let mut thread = thread_with_initial_turns(response)?;
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .context("thread/resume 응답에 thread.id가 없습니다.")?
        .to_owned();
    let mut cursor = None;
    let mut turns = Vec::new();

    loop {
        let page = server
            .request("thread/turns/list", turns_list_params(&thread_id, cursor.as_deref()))
            .await?;
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .context("thread/turns/list 응답에 data가 없습니다.")?;
        turns.extend(data.iter().cloned());
        let Some(next) = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            break;
        };
        cursor = Some(next);
    }
    thread
        .as_object_mut()
        .expect("thread was validated as an object")
        .insert("turns".to_owned(), Value::Array(turns));
    Ok(thread)
}

fn turns_list_params(thread_id: &str, cursor: Option<&str>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "limit": 100,
        "sortDirection": "asc",
        "itemsView": "full"
    });
    if let Some(cursor) = cursor {
        params["cursor"] = json!(cursor);
    }
    params
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
                Ok(()) => state.set_copy_notice(),
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
        Action::ScrollToBottom => {
            renderer.scroll_to_bottom();
        }
        // `Action::None`, `Action::Tick`, and anything routed here by mistake: the
        // callers only ever pass the variants above, so silently doing nothing is
        // safer than panicking inside the render loop.
        _ => {}
    }
    Ok(false)
}

async fn set_fast_mode(server: &BackendServer, state: &mut AppState, enabled: bool) {
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
            if state.effective_fast_mode() != enabled {
                state.set_fast_mode(enabled);
            }
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
        Err(error) => state.push_notice(BlockKind::Error, "Fast 전환 실패", error.to_string()),
    }
}

async fn set_claude_permission_mode(
    server: &BackendServer,
    state: &mut AppState,
    mode: state::ClaudePermissionMode,
) {
    if let Err(error) = server
        .request(
            "thread/permissionMode/set",
            json!({ "threadId": state.thread_id, "permissionMode": mode.wire() }),
        )
        .await
    {
        state.push_notice(BlockKind::Warning, "권한 모드 전환 실패", error.to_string());
    }
    if let Err(error) = server
        .request(
            "config/value/write",
            config_value_write_params(state::CLAUDE_PERMISSION_MODE_KEY, mode.wire()),
        )
        .await
    {
        state.push_notice(
            BlockKind::Warning,
            "권한 모드 기본값 저장 실패",
            error.to_string(),
        );
    }
}

async fn persist_vibe_display_modes(
    server: &BackendServer,
    state: &mut AppState,
    vibe: VibeMode,
    response: state::ResponseLength,
    shell: ShellDisplayMode,
    diff: DiffDisplayMode,
) {
    for (key, value) in [
        ("vibe_mode", vibe.config_value()),
        ("model_verbosity", response.model_verbosity()),
        ("shell_display_mode", shell.config_value()),
        ("diff_display_mode", diff.config_value()),
    ] {
        if let Err(error) = server
            .request("config/value/write", config_value_write_params(key, value))
            .await
        {
            state.push_notice(BlockKind::Warning, "Vibe 표시 설정 저장 실패", error.to_string());
            break;
        }
    }
}

fn config_value_write_params(key_path: &str, value: &str) -> Value {
    json!({
        "keyPath": key_path,
        "value": value,
        "mergeStrategy": "upsert"
    })
}

/// Sent as `developerInstructions` on every thread Devez Vibe starts, so these
/// rules hold for every user without any per-machine configuration.
const DEVEZ_INSTRUCTIONS: &str = concat!(
    "Updated Plan의 설명과 모든 Task 제목은 반드시 자연스러운 한국어로 작성한다. ",
    "코드, 명령어, 경로, 제품명 등 기술 식별자는 원문을 유지한다.\n",
    "답변 형식 규칙:\n",
    "- 서론, 인사, 맺음말 요약을 쓰지 않고 결론부터 쓴다.\n",
    "- 기본 분량은 세 줄 전후이며, 사용자가 자세한 설명을 요청할 때만 늘린다.\n",
    "- 다만 사용자에게 선택이나 승인을 요청하는 답변에는 이 분량 제한을 적용하지 않는다. ",
    "고를 수 있는 선택지, 각 선택지의 결과, 판단에 필요한 사실을 하나도 빠뜨리지 않고 적고, ",
    "분량을 맞추려고 선택지를 줄이거나 문장을 도중에 끊지 않는다. ",
    "마지막 줄에서 무엇을 선택하면 되는지 한 문장으로 묻는다.\n",
    "- 산문 문단 대신 불릿과 코드 블록을 쓴다.\n",
    "- 코드 변경은 파일 경로와 핵심 코드만 보여주고, 요청받지 않은 해설을 덧붙이지 않는다.\n",
    "- Super Vibe 모드에서는 파일 경로, 코드 블록, 함수·클래스·변수·설정 키 이름을 답변에 넣지 않는다. ",
    "무엇을 어떻게 바꿨는지 일상 언어로만 설명하고, 계획이나 작업 단계를 답변 본문에 다시 나열하지 않는다. ",
    "사용자가 코드나 경로를 직접 요청했거나, 그것 없이는 사용자가 판단할 수 없는 경우에만 보여준다.\n",
    "- 파일 수정, 명령 실행처럼 실제로 무언가를 바꾼 작업을 마쳤을 때만 마지막 문장을 완료 보고로 쓴다. ",
    "질문에 답하거나 조사·설명만 한 응답, 사용자의 제안을 거절하거나 확인만 한 응답에는 완료 문구를 붙이지 않는다.\n",
    "- 완료 보고는 수행한 동작을 그대로 목적어로 삼아 `~했습니다.`로 끝낸다. 예: `임시 파일 정리 주기를 변경했습니다.` ",
    "`~한 내용을 완료했습니다.`처럼 명사절을 겹쳐 쓰거나 조사한 사실을 완료한 것처럼 적지 않는다.\n",
    "- 하지 않기로 한 선택지나 이미 정해진 결정을 다시 나열하지 않는다.\n",
    "내용 정확성 규칙:\n",
    "- 사용자의 핵심 질문을 먼저 확정하고, 최종 답변의 결론은 실제로 확인한 근거에만 기반한다.\n",
    "- 저장소의 사실이나 원인을 조사할 때는 첫 검색 결과나 단일 키워드에 의존하지 않는다. 관련 상태·표시·입력 흐름을 추적하고, 적절한 테스트 또는 변경 이력과 교차 확인한다.\n",
    "- 검색에서 찾지 못했다는 이유만으로 기능이나 코드가 없다고 단정하지 않는다. 현재 구현, 과거 문제의 원인, 추측을 구분하고 근거가 부족하면 미확인이라고 밝힌다.\n",
    "- 최종 답변에는 직접적인 결론, 이를 뒷받침하는 핵심 근거, 확인 범위나 한계만 우선해서 담는다. 읽기 전용 수행 여부나 내부 절차는 결과 판단에 필요할 때만 언급한다.\n",
    "- Skill 적용, 지침 확인, 내부 도구 호출 같은 내부 절차를 사용자에게 commentary로 알리지 않는다. ",
    "사용자 판단에 필요한 진행 상황이나 결과만 알린다.\n",
    "진행 보고 규칙:\n",
    "- 단순 질문이 아닌 작업은 첫 도구 호출 전에 무엇을 확인하고 이어서 무엇을 할지 한두 문장으로 알린다. ",
    "이후에는 새 사실이 사용자 판단을 바꾸거나 작업 범위가 달라질 때만 짧게 알리고, 같은 내용을 반복하지 않는다.\n",
    "- 무엇을 알아냈는지 담기지 않은 진행 문장은 쓰지 않는다. ",
    "`다음 부분을 이어서 확인하겠습니다.`, `이어서 진행하겠습니다.`, `계속 확인하겠습니다.`처럼 ",
    "다음에 무엇을 왜 보는지 없는 문장은 같은 응답에서 한 번도 쓰지 않는다.\n",
    "계획 규칙:\n",
    "- 실행 단계가 두 개 이상이거나 도구를 두 번 이상 호출할 작업, 설계 판단이 필요한 작업에서는 반드시 `update_plan`으로 짧은 계획을 먼저 세운다.\n",
    "- 단순 질문, 도구 한 번의 조회, 한 줄 수정처럼 바로 끝나는 요청에는 계획을 만들지 않는다.\n",
    "- 모든 Task 제목은 순서대로 `1. `, `2. `, `3. `처럼 번호로 시작한다.\n",
    "- Task에는 실제 조사·수정·검증 작업만 넣고, 결론 정리나 완료 보고만을 별도 Task로 만들지 않는다.\n",
    "- 각 Task는 착수할 때 in_progress, 끝나면 completed로 즉시 갱신한다.\n",
    "- 계획을 만들었다면 동시에 in_progress인 Task는 하나만 두고, 현재 Task를 completed로 바꾼 뒤 다음 Task를 in_progress로 바꾼다.\n",
    "- 종료 직전에 여러 Task를 한꺼번에 completed로 바꾸지 않는다. 각 Task의 첫 작업 도구를 호출하기 전에 해당 Task를 in_progress로 바꾸고, 그 작업이 끝난 직후 completed로 바꾼다.\n",
);

/// Claude Code already owns its native task system. These rules preserve the
/// same visible workflow while naming the Claude tools it can actually call.
const CLAUDE_DEVEZ_INSTRUCTIONS: &str = concat!(
    "Devez Vibe에서 작업한다. Task 목록의 설명과 모든 Task 제목은 반드시 자연스러운 한국어로 작성한다. ",
    "코드, 명령어, 경로, 제품명 등 기술 식별자는 원문을 유지한다.\n",
    "최우선 언어 규칙: 사용자에게 보이는 모든 일반 문장은 반드시 한국어로 작성한다. ",
    "진행 안내, 조사 중 알림, 도구 호출 전후 text, 최종 답변을 포함하며 영어 문장은 예외 없이 금지한다. ",
    "영어는 코드, 명령어, 경로, 제품명 등 기술 식별자와 사용자가 그대로 인용한 문자열에만 허용한다. ",
    "문장을 출력하기 직전에 한국어가 아닌 자연어 문장이 없는지 확인하고, 있으면 자연스러운 한국어로 바꾼다.\n",
    "최우선 영어 라벨 금지 규칙: 사용자에게 보이는 모든 문장은 첫 글자부터 한국어여야 한다. ",
    "`Now`, `Let me`, `I'll`, `Next`, `First`, `Okay`, `Alright`, `Fine`처럼 영어 낱말로 문장을 시작하지 않는다. ",
    "이는 도구 호출 앞에 붙이는 한 줄짜리 진행 라벨에도 똑같이 적용되며, ",
    "`Now paint_screen과 애니메이션 경로에 패널을 그립니다.`처럼 영어 낱말 뒤에 한국어를 이어 붙이는 형태가 가장 흔한 위반이다. ",
    "이런 문장은 영어 낱말을 지우고 한국어만 남긴다. 예: `Now 토글 함수를 넣습니다.` → `토글 함수를 넣습니다.`\n",
    "최우선 시작 응답 규칙: 단순 질문이 아닌 작업에서는 첫 응답 content block을 반드시 사용자에게 보이는 짧은 진행 안내 text로 출력한다. ",
    "TaskCreate를 포함한 어떤 tool_use도 이 text보다 먼저 출력하지 않는다. 같은 assistant message에 text와 tool_use를 함께 출력할 때도 text를 앞에 둔다. ",
    "진행 안내에는 요청에서 무엇을 먼저 확인하고 이어서 무엇을 할지 사용자의 언어로 한두 문장만 적는다. ",
    "이 규칙은 사용자 메시지에 대한 첫 assistant message에만 적용한다. ",
    "그다음부터는 알릴 새 사실이 없으면 tool_use 앞에 text를 붙이지 않고 도구를 바로 호출한다.\n",
    "최우선 작업 단계 규칙: Read, Grep, Glob, Bash 등 작업 도구를 두 번 이상 호출할 가능성이 있으면 다른 도구보다 먼저 TaskCreate를 호출한다. ",
    "TaskCreate 없이 두 번째 작업 도구를 호출하면 지침 위반이다. 모든 TaskCreate가 끝나면 다른 작업 도구보다 먼저 첫 Task를 TaskUpdate로 `in_progress`로 바꾼다. ",
    "모든 Task는 예외 없이 `pending` → `in_progress` → `completed` 순서로 바꾸며 `pending`에서 `completed`로 바로 바꾸지 않는다. ",
    "현재 Task를 `completed`로 바꾼 뒤 다음 Task를 `in_progress`로 바꾸고 해당 작업을 시작한다.\n",
    "답변 형식 규칙:\n",
    "- 서론, 인사, 맺음말 요약을 쓰지 않고 결론부터 쓴다.\n",
    "- 기본 분량은 세 줄 전후이며, 사용자가 자세한 설명을 요청할 때만 늘린다.\n",
    "- 산문 문단 대신 불릿과 코드 블록을 쓴다.\n",
    "- 코드 변경은 파일 경로와 핵심 코드만 보여주고, 요청받지 않은 해설을 덧붙이지 않는다.\n",
    "- Super Vibe 모드에서는 파일 경로, 코드 블록, 함수·클래스·변수·설정 키 이름을 답변에 넣지 않는다. ",
    "무엇을 어떻게 바꿨는지 일상 언어로만 설명하고, 계획이나 작업 단계를 답변 본문에 다시 나열하지 않는다. ",
    "사용자가 코드나 경로를 직접 요청했거나, 그것 없이는 사용자가 판단할 수 없는 경우에만 보여준다.\n",
    "- Vibe와 Super Vibe 모드의 최종 답변은 반드시 불릿 두 개 이하, 전체 200자 이내, 불릿마다 한 문장으로 쓴다. ",
    "사용자가 자세한 설명을 명시적으로 요청한 경우에만 늘린다.\n",
    "- 사용자에게 선택이나 승인을 요청할 때는 본문에 선택지를 나열하지 말고 반드시 AskUserQuestion 도구로 묻는다. ",
    "각 선택지의 label에는 고를 대상을, description에는 그 선택의 결과와 판단에 필요한 사실을 적는다. ",
    "서로 배타적이지 않은 선택에는 multiSelect를 켜고, 한 번에 정해야 할 것이 여러 가지면 질문을 나눠 함께 묻는다. ",
    "`기타` 선택지는 자동으로 제공되므로 직접 만들지 않는다.\n",
    "- 선택지가 다섯 개 이상이라 AskUserQuestion에 담기지 않을 때만 본문에 글로 나열한다. ",
    "이때는 분량 제한을 적용하지 않고, 선택지와 각각의 결과를 하나도 빠뜨리지 않고 적은 뒤 ",
    "마지막 줄에서 무엇을 선택하면 되는지 한 문장으로 묻는다.\n",
    "- 파일 수정, 명령 실행처럼 실제로 무언가를 바꾼 작업을 마쳤을 때만 마지막 불릿을 완료 보고로 쓴다. ",
    "질문에 답하거나 조사·설명만 한 응답, 사용자의 제안을 거절하거나 확인만 한 응답에는 완료 문구를 붙이지 않는다.\n",
    "- 완료 보고는 수행한 동작을 그대로 목적어로 삼아 `~했습니다.`로 끝낸다. 예: `임시 파일 정리 주기를 변경했습니다.` ",
    "`~한 내용을 완료했습니다.`처럼 명사절을 겹쳐 쓰거나 조사한 사실을 완료한 것처럼 적지 않는다.\n",
    "- 하지 않기로 한 선택지나 이미 정해진 결정을 다시 나열하지 않는다.\n",
    "내용 정확성 규칙:\n",
    "- 사용자의 핵심 질문을 먼저 확정하고, 최종 답변의 결론은 실제로 확인한 근거에만 기반한다.\n",
    "- 저장소의 사실이나 원인을 조사할 때는 첫 검색 결과나 단일 키워드에 의존하지 않는다. 관련 상태·표시·입력 흐름을 추적하고, 적절한 테스트 또는 변경 이력과 교차 확인한다.\n",
    "- 검색에서 찾지 못했다는 이유만으로 기능이나 코드가 없다고 단정하지 않는다. 현재 구현, 과거 문제의 원인, 추측을 구분하고 근거가 부족하면 미확인이라고 밝힌다.\n",
    "- 최종 답변에는 직접적인 결론, 이를 뒷받침하는 핵심 근거, 확인 범위나 한계만 우선해서 담는다. 읽기 전용 수행 여부나 내부 절차는 결과 판단에 필요할 때만 언급한다.\n",
    "- 사용자에게 보이는 진행 안내와 답변은 항상 한국어로 작성한다. ",
    "사용자가 영어로 요청해도 Devez Vibe의 응답 언어는 한국어로 유지한다. ",
    "`Now ...` 같은 독립된 영어 진행 문장은 도구 호출 사이를 포함해 절대 출력하지 않는다. ",
    "`I'll check ...`, `Fine. Building ...`, `Now the tile view logic.` 또는 `Now the filter builder.`도 금지한다.\n",
    "진행 보고 규칙:\n",
    "- 단순 질문이 아닌 작업은 첫 도구 호출 전에 무엇을 확인하고 이어서 무엇을 할지 한두 문장으로 알린다. ",
    "이후에는 새 사실이 사용자 판단을 바꾸거나 작업 범위가 달라질 때만 짧게 알리고, 같은 내용을 반복하지 않는다.\n",
    "- 무엇을 알아냈는지 담기지 않은 진행 문장은 쓰지 않는다. ",
    "`다음 부분을 이어서 확인하겠습니다.`, `이어서 진행하겠습니다.`, `계속 확인하겠습니다.`처럼 ",
    "다음에 무엇을 왜 보는지 없는 문장은 같은 응답에서 한 번도 쓰지 않는다.\n",
    "- 완료 보고는 세 줄 이내로 쓰며, 검증하지 못한 내용은 짧게 밝힌다.\n",
    "- Skill 적용, 지침 확인, 내부 도구 호출 같은 내부 절차는 알리지 않는다.\n",
    "작업 단계 규칙:\n",
    "- 실행 단계가 두 개 이상이거나 도구를 두 번 이상 호출할 작업, 설계 판단이 필요한 작업에서는 반드시 첫 도구 호출 전에 Claude Code의 TaskCreate로 짧은 작업 목록을 만든다.\n",
    "- 단순 질문, 도구 한 번의 조회, 한 줄 수정처럼 바로 끝나는 요청에는 Task를 만들지 않는다.\n",
    "- 모든 Task의 subject는 순서대로 `1. `, `2. `, `3. `처럼 번호로 시작한다. ",
    "번호는 새 작업 목록마다 항상 `1. `부터 다시 시작하고, 이전 작업 목록에서 쓴 번호에 이어 붙이지 않는다. ",
    "TaskList에 이미 끝난 Task가 남아 있어도 그 번호를 이어받지 않는다.\n",
    "- Task에는 실제 조사·수정·검증 작업만 넣고, `결론 정리`, `결과 보고`, `완료 보고`만을 별도 Task로 만들지 않는다.\n",
    "- 각 Task는 착수 즉시 TaskUpdate로 `in_progress`, 끝나면 즉시 `completed`로 바꾼다.\n",
    "- 동시에 `in_progress`인 Task는 하나만 두고, 현재 Task를 `completed`로 바꾼 뒤 다음 Task를 `in_progress`로 바꾼다.\n",
    "- 종료 직전에 여러 Task를 한꺼번에 `completed`로 바꾸지 않는다. 각 Task의 첫 Read, Grep, Glob, Bash 등 작업 도구를 호출하기 전에 그 Task를 `in_progress`로 바꾸고, 해당 작업이 끝난 직후 `completed`로 바꾼다.\n",
);

/// The Claude selections a session has to be told, because the bridge opens a
/// fresh SDK session for every start and resume. Anything left out here comes
/// back as the SDK's own default, which is what used to reset the model, the
/// effort and the permission badge on each `/resume`.
struct ClaudeSessionSettings {
    /// Only a fallback: a resumed thread reopens on the model its own turns ran
    /// on, and this saved default is what a thread with no such record gets.
    /// Empty while a Codex model is selected, since forcing a non-Claude id on a
    /// Claude session would leave it with no model at all.
    model: String,
    effort: String,
    permission_mode: String,
}

fn claude_session_settings(state: &AppState) -> ClaudeSessionSettings {
    let claude_model = claude::is_claude_model(state.selected_model_name());
    ClaudeSessionSettings {
        model: claude_model
            .then(|| state.selected_model_name().to_owned())
            .unwrap_or_default(),
        effort: claude_model
            .then(|| state.selected_effort().to_owned())
            .unwrap_or_default(),
        permission_mode: state
            .claude_permission_mode_setting()
            .wire()
            .to_owned(),
    }
}

/// A resumed thread replays the `developer` message its rollout was recorded
/// with, so the rules have to be re-sent or an old session keeps running on an
/// older wording. The Claude selections ride along for the same reason: a
/// resumed session would otherwise reopen on the SDK defaults.
fn resume_thread_params(thread_id: &str, claude: &ClaudeSessionSettings) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "developerInstructions": DEVEZ_INSTRUCTIONS,
        "claudeDeveloperInstructions": CLAUDE_DEVEZ_INSTRUCTIONS,
        "claudePermissionMode": claude.permission_mode,
        "initialTurnsPage": {
            "limit": 100,
            "sortDirection": "asc",
            "itemsView": "full"
        }
    });
    // Sent as fallbacks, not as the choice: the backend prefers what this
    // thread's own turns ran on, and only reaches for these when it has no
    // record. `model`/`effort` stay free for an explicit `--model`/`--effort`.
    if !claude.model.is_empty() {
        params["claudeFallbackModel"] = json!(claude.model);
    }
    if !claude.effort.is_empty() {
        params["claudeFallbackEffort"] = json!(claude.effort);
    }
    params
}

/// One `developer` message at the head of the thread loses its grip as turns
/// pile up, so the same rules ride along with every turn. The active preset
/// rides along too: the rules that depend on it are written as conditions, and
/// the preset is a local display setting the provider is told nothing else about.
fn turn_additional_context(vibe: VibeMode) -> Value {
    json!({
        "devez-vibe-rules": {
            "value": DEVEZ_INSTRUCTIONS,
            "kind": "application"
        },
        "claude-devez-vibe-rules": {
            "value": CLAUDE_DEVEZ_INSTRUCTIONS,
            "kind": "application"
        },
        "devez-vibe-mode": {
            "value": vibe.turn_notice(),
            "kind": "application"
        }
    })
}

fn new_thread_params(
    cwd: &str,
    model: Option<&str>,
    service_tier: Option<&str>,
    session_start_source: &str,
    model_verbosity: &str,
    claude_permission_mode: &str,
    effort: &str,
) -> Value {
    let mut params = json!({
        "cwd": cwd,
        "permissions": ":danger-full-access",
        "config": { "model_verbosity": model_verbosity },
        "developerInstructions": DEVEZ_INSTRUCTIONS,
        "claudeDeveloperInstructions": CLAUDE_DEVEZ_INSTRUCTIONS,
        "sessionStartSource": session_start_source,
        "threadSource": "devez-vibe"
    });
    if let Some(model) = model {
        params["model"] = json!(model);
    }
    if let Some(service_tier) = service_tier {
        params["serviceTier"] = json!(service_tier);
    }
    if !claude_permission_mode.is_empty() {
        params["claudePermissionMode"] = json!(claude_permission_mode);
    }
    // Without this the runtime starts on its own default effort and the reply
    // overwrites the effort the first frame already showed.
    if !effort.is_empty() {
        params["effort"] = json!(effort);
    }
    params
}

async fn list_skills(server: &BackendServer, cwd: &str, force_reload: bool) -> Result<Value> {
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

async fn list_plugins(server: &BackendServer, cwd: &str) -> Result<Value> {
    server
        .request(
            "plugin/list",
            json!({
                "cwds": [cwd]
            }),
        )
        .await
}

async fn list_mcp_servers(server: &BackendServer, thread_id: &str) -> Result<Value> {
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

async fn refresh_provider_models(
    server: &BackendServer,
    state: &mut AppState,
    provider_name: &str,
) {
    match server
        .request(
            "model/list",
            json!({ "includeHidden": false, "limit": 100 }),
        )
        .await
    {
        Ok(response) => {
            state.replace_models(parse_models(&response));
            state.provider_connected(provider_name);
        }
        Err(error) => {
            state.provider_connected(provider_name);
            state.push_notice(
                BlockKind::Warning,
                "모델 새로고침 실패",
                error.to_string(),
            );
        }
    }
}

async fn write_plugin_enabled(server: &BackendServer, plugin_id: &str, enabled: bool) -> Result<Value> {
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
    server: &BackendServer,
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

async fn reopen_marketplaces(server: &BackendServer, state: &mut AppState, notice: String) {
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

struct IntegrationCatalog {
    skills: std::result::Result<Value, String>,
    plugins: std::result::Result<Value, String>,
    apps: std::result::Result<Value, String>,
}

async fn fetch_integrations(
    client: Option<app_server::AppServerClient>,
    cwd: String,
    app_thread_id: Option<String>,
    force_reload: bool,
) -> IntegrationCatalog {
    let Some(client) = client else {
        return IntegrationCatalog {
            skills: Ok(json!({ "data": [] })),
            plugins: Ok(json!({ "data": [] })),
            apps: Ok(json!({ "data": [] })),
        };
    };
    let skills_client = client.clone();
    let plugins_client = client.clone();
    let apps = async {
        let Some(thread_id) = app_thread_id else {
            return Ok(json!({ "data": [] }));
        };
        client
            .request(
                "app/list",
                json!({
                    "cursor": null,
                    "limit": 100,
                    "threadId": thread_id,
                    "forceRefetch": force_reload
                }),
            )
            .await
    };
    let (skills, plugins, apps) = tokio::join!(
        skills_client.request(
            "skills/list",
            json!({
                "cwds": [cwd],
                "forceReload": force_reload
            }),
        ),
        plugins_client.request(
            "plugin/installed",
            json!({
                "cwds": [cwd]
            }),
        ),
        apps,
    );
    IntegrationCatalog {
        skills: skills.map_err(|error| error.to_string()),
        plugins: plugins.map_err(|error| error.to_string()),
        apps: apps.map_err(|error| error.to_string()),
    }
}

fn apply_integrations(state: &mut AppState, catalog: IntegrationCatalog) -> Result<()> {
    let mut errors = Vec::new();
    match catalog.skills {
        Ok(response) => state.update_skills(&response),
        Err(error) => errors.push(format!("Skill 조회 실패: {error}")),
    }
    match catalog.plugins {
        Ok(response) => state.update_plugins(&response),
        Err(error) => errors.push(format!("플러그인 조회 실패: {error}")),
    }
    match catalog.apps {
        Ok(response) => state.update_apps(&response),
        Err(error) => errors.push(format!("App 조회 실패: {error}")),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

fn start_background_catalogue(
    catalogue: impl Future<Output = IntegrationCatalog> + Send + 'static,
) -> mpsc::Receiver<IntegrationCatalog> {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        let _ = sender.send(catalogue.await).await;
    });
    receiver
}

struct CostRestore {
    thread_id: String,
    ledger: Option<pricing::CostLedger>,
}

fn start_cost_restore(thread_id: String) -> mpsc::Receiver<CostRestore> {
    let lookup_thread_id = thread_id.clone();
    start_background_cost_restore(thread_id, move || {
        state::codex_home()
            .and_then(|home| rollout::load_cost_ledger(&home, &lookup_thread_id))
    })
}

fn start_background_cost_restore(
    thread_id: String,
    restore: impl FnOnce() -> Option<pricing::CostLedger> + Send + 'static,
) -> mpsc::Receiver<CostRestore> {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        let ledger = tokio::task::spawn_blocking(restore)
            .await
            .ok()
            .flatten();
        let _ = sender.send(CostRestore { thread_id, ledger }).await;
    });
    receiver
}

async fn recv_cost_restore(
    receiver: &mut Option<mpsc::Receiver<CostRestore>>,
) -> Option<CostRestore> {
    let Some(channel) = receiver.as_mut() else {
        return std::future::pending().await;
    };
    let restored = channel.recv().await;
    if restored.is_none() {
        *receiver = None;
    }
    restored
}

fn start_integration_refresh(
    server: &BackendServer,
    state: &AppState,
) -> mpsc::Receiver<IntegrationCatalog> {
    let app_thread_id = integration_app_thread_id(server, state);
    start_background_catalogue(fetch_integrations(
        server.client(),
        state.cwd.clone(),
        app_thread_id,
        false,
    ))
}

async fn recv_integrations(
    receiver: &mut Option<mpsc::Receiver<IntegrationCatalog>>,
) -> Option<IntegrationCatalog> {
    let Some(channel) = receiver.as_mut() else {
        return std::future::pending().await;
    };
    let catalogue = channel.recv().await;
    if catalogue.is_none() {
        *receiver = None;
    }
    catalogue
}

async fn refresh_integrations(
    server: &BackendServer,
    state: &mut AppState,
    force_reload: bool,
) -> Result<()> {
    let app_thread_id = integration_app_thread_id(server, state);
    let catalog = fetch_integrations(
        server.client(),
        state.cwd.clone(),
        app_thread_id,
        force_reload,
    )
    .await;
    apply_integrations(state, catalog)
}

fn integration_app_thread_id(server: &BackendServer, state: &AppState) -> Option<String> {
    app_thread_id_for_model(
        state.selected_model_name(),
        server.codex_thread_id(&state.thread_id),
    )
}

fn app_thread_id_for_model(model: &str, codex_thread_id: Option<String>) -> Option<String> {
    (!claude::is_claude_model(model) && !open_code::is_open_code_model(model))
        .then_some(codex_thread_id)
        .flatten()
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

fn provider_handoff_snapshot(state: &AppState, renderer: &Renderer) -> Value {
    let mut blocks = renderer.provider_handoff_blocks();
    for pending in state.pending_provider_handoff_blocks() {
        if let Some(existing) = blocks.iter_mut().find(|block| block.id == pending.id) {
            *existing = pending;
        } else {
            blocks.push(pending);
        }
    }
    let entries = blocks
        .into_iter()
        .map(|block| {
            json!({
                "id": block.id,
                "kind": block.kind,
                "title": block.title,
                "body": block.body
            })
        })
        .collect::<Vec<_>>();
    json!({
        "lastBlockId": renderer
            .last_history_block_id()
            .max(state.last_pending_handoff_block_id()),
        "cwd": state.cwd,
        "plan": state.provider_handoff_plan(),
        "entries": entries
    })
}

async fn start_turn(
    server: &BackendServer,
    state: &mut AppState,
    text: String,
    provider_handoff: Option<Value>,
) {
    devezcode::note_prompt(&text);
    let model = state.selected_model_name().to_owned();
    let effort = state.selected_effort().to_owned();
    state.note_pending_turn_model(&model);
    state.note_pending_turn_effort(&effort);
    let input = state.turn_input(text);
    let mut params = json!({
        "threadId": state.thread_id,
        "input": input,
        "model": model,
        "serviceTier": state.service_tier(),
        "permissions": state.permission_profile(),
        "additionalContext": turn_additional_context(state.vibe_mode())
    });
    if !effort.is_empty() {
        params["effort"] = json!(effort);
    }
    if let Some(mode) = state.claude_permission_mode() {
        params["claudePermissionMode"] = json!(mode.wire());
    }
    if let Some(provider_handoff) = provider_handoff {
        params["providerHandoff"] = provider_handoff;
    }
    match server.request("turn/start", params).await {
        // The response reserves an id, but the app-server makes it
        // interruptible only after the subsequent `turn/started` notification.
        Ok(_) => {}
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

/// `Ctrl+V` on a composer already showing a collapsed paste expands it instead
/// of pasting the same block again, the way Claude Code's composer does. The
/// clipboard is read here so the decision never depends on whether the terminal
/// forwards the payload as key records, as bracketed paste, or as both.
fn expand_collapsed_paste_shortcut(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: &KeyEvent,
    now: Instant,
) -> bool {
    if !is_paste_shortcut(key) || state.has_pending_interaction() {
        return false;
    }
    let Some(block) = state.editor.collapsed_paste_text() else {
        return false;
    };
    if !clipboard_text().is_some_and(|text| {
        paste::paste_payload_chars(&text) == paste::paste_payload_chars(&block)
    }) {
        return false;
    }
    state.editor.expand_collapsed_paste();
    // The keys the terminal is about to synthesize are that same block; they
    // would otherwise land as a second paste, and its newlines as submits.
    buffer.discard_expected(&block, now);
    true
}

fn clipboard_text() -> Option<String> {
    Clipboard::new().ok()?.get_text().ok()
}

fn is_paste_shortcut(key: &KeyEvent) -> bool {
    matches!(key.kind, crossterm::event::KeyEventKind::Press)
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_clipboard_image_shortcut(key: &KeyEvent) -> bool {
    matches!(key.kind, crossterm::event::KeyEventKind::Press)
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT))
}

/// Backspace and Delete on their own: with composer text drag-selected, they take
/// the selection. Modified chords keep the meanings they already have, so
/// Ctrl+Backspace stays a word delete.
fn is_selection_delete_key(key: &KeyEvent) -> bool {
    matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        && !key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
        )
}

fn attach_pasted_local_image(state: &mut AppState, text: &str) -> bool {
    let Some(path) = local_image_path_from_paste(text) else {
        return false;
    };
    state.attach_local_image(path.to_string_lossy().into_owned());
    true
}

fn apply_composer_text(state: &mut AppState, text: BufferedText) {
    match text.target {
        BufferedTextTarget::PendingUserInput(target) => {
            state.handle_buffered_prompt_text(&target, &text.text);
        }
        BufferedTextTarget::Composer if !text.pasted => {
            state.handle_buffered_composer_text(&text.text, false);
        }
        BufferedTextTarget::Composer if !attach_pasted_local_image(state, &text.text) => {
            state.handle_buffered_composer_text(&text.text, true);
        }
        BufferedTextTarget::Composer => {}
    }
}

/// A real bracketed-paste event has no classification delay, so it belongs to
/// whichever control is focused now. Only synthesized key runs need a captured
/// target carried through the buffer.
fn apply_direct_paste(state: &mut AppState, text: &str) {
    if state.has_pending_interaction() || !attach_pasted_local_image(state, text) {
        state.handle_paste(text);
    }
}

fn apply_composer_inputs(state: &mut AppState, inputs: Vec<ComposerInput>) -> Action {
    let mut action = Action::None;
    for input in inputs {
        action = match input {
            ComposerInput::Key(key) => state.handle_key(key),
            ComposerInput::Text(text) => {
                apply_composer_text(state, text);
                Action::None
            }
        };
    }
    action
}

/// Windows Terminal pastes by synthesizing key records and keeps the `Ctrl+V`
/// to itself, so the only announcement a second paste gets is its first
/// character. When that character opens the block the composer is showing
/// collapsed, the clipboard is read and compared: a match makes the run a paste
/// by evidence rather than by how fast its keys arrive, which is what stops one
/// of its newlines from reaching the composer as a submit key.
fn arm_verified_collapsed_paste(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: &KeyEvent,
    now: Instant,
) {
    if buffer.is_buffering() {
        return;
    }
    let Some(block) = state.editor.collapsed_paste_text() else {
        return;
    };
    let payload = paste::paste_payload_chars(&block);
    if payload.is_empty() || payload.first().copied() != paste::payload_char(key) {
        return;
    }
    if clipboard_text().is_none_or(|text| paste::paste_payload_chars(&text) != payload) {
        return;
    }
    buffer.expect_verified_paste(&block, now);
}

fn observe_composer_key(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: KeyEvent,
    now: Instant,
) -> Action {
    // Windows reports key-up records while its IME still owns the visible
    // preedit. They change no editor state, and repainting for one replaces the
    // preedit with our placeholder before the composed syllable can commit.
    if key.kind == KeyEventKind::Release {
        return Action::Tick(false);
    }
    if let Some(target) = state.pending_text_input_target() {
        apply_composer_inputs(
            state,
            buffer.observe_targeted(
                key,
                now,
                BufferedTextTarget::PendingUserInput(target),
            ),
        )
    } else if state.has_pending_interaction() {
        state.handle_key(key)
    } else {
        arm_verified_collapsed_paste(state, buffer, &key, now);
        let expected_paste = state.editor.collapsed_paste_text();
        apply_composer_inputs(
            state,
            buffer.observe_expected(key, now, expected_paste.as_deref()),
        )
    }
}

fn apply_composer_inputs_with_scroll(
    state: &mut AppState,
    renderer: &mut Renderer,
    inputs: Vec<ComposerInput>,
) -> Action {
    let mut action = Action::None;
    for input in inputs {
        action = match input {
            ComposerInput::Key(key) => match scroll_request(renderer, &key) {
                Some(delta) => Action::Tick(renderer.scroll(delta)),
                None => state.handle_key(key),
            },
            ComposerInput::Text(text) => {
                apply_composer_text(state, text);
                Action::None
            }
        };
    }
    action
}

fn observe_composer_key_with_scroll(
    state: &mut AppState,
    renderer: &mut Renderer,
    buffer: &mut ComposerPasteBuffer,
    key: KeyEvent,
    now: Instant,
) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Tick(false);
    }
    if let Some(target) = state.pending_text_input_target() {
        apply_composer_inputs_with_scroll(
            state,
            renderer,
            buffer.observe_targeted(
                key,
                now,
                BufferedTextTarget::PendingUserInput(target),
            ),
        )
    } else if state.has_pending_interaction() {
        state.handle_key(key)
    } else {
        arm_verified_collapsed_paste(state, buffer, &key, now);
        let expected_paste = state.editor.collapsed_paste_text();
        apply_composer_inputs_with_scroll(
            state,
            renderer,
            buffer.observe_expected(key, now, expected_paste.as_deref()),
        )
    }
}

fn flush_composer_paste(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    now: Instant,
) -> bool {
    if let Some(text) = buffer.flush_if_idle(now) {
        apply_composer_text(state, text);
        true
    } else {
        false
    }
}

fn local_image_path_from_paste(text: &str) -> Option<PathBuf> {
    let path = PathBuf::from(text.trim());
    if !path.is_file() {
        return None;
    }
    let mut header = [0; 12];
    let bytes = fs::File::open(&path).ok()?.read(&mut header).ok()?;
    let image = header[..bytes].starts_with(b"\x89PNG\r\n\x1a\n")
        || header[..bytes].starts_with(&[0xff, 0xd8, 0xff])
        || header[..bytes].starts_with(b"GIF87a")
        || header[..bytes].starts_with(b"GIF89a")
        || header[..bytes].starts_with(b"BM")
        || (bytes >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP");
    image.then_some(path)
}

fn write_clipboard_bmp(image: &ImageData<'_>) -> std::io::Result<PathBuf> {
    let directory = env::temp_dir().join("devez-vibe-images");
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
        for rgba in image.bytes[row * image.width * 4..(row + 1) * image.width * 4].chunks_exact(4)
        {
            bmp.extend_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
    fs::write(&path, bmp)?;
    Ok(path)
}

fn draw(state: &mut AppState, renderer: &mut Renderer) -> Result<()> {
    let draw_started = Instant::now();
    // Every state change the user can see reaches a frame, so the host's copy of
    // the session state is refreshed from the same place rather than from each
    // of the call sites that can move it.
    // The turn flag and compaction are handed over separately: the host spins its
    // tab for both, but only a finished turn is a finished response.
    devezcode::sync(
        state.host_session_id(),
        state.busy,
        state.compacting(),
        state.host_loading(),
        state.awaiting_input(),
    );
    let committed = state.drain_committed();
    let view = state.view();
    let view_elapsed = draw_started.elapsed();
    let live_blocks = view.live_blocks.len();
    let live_bytes = view
        .live_blocks
        .iter()
        .map(|live| live.block.title.len() + live.block.body.len())
        .sum();
    let render_started = Instant::now();
    let result = renderer.render(&committed, view);
    perf::record_draw(
        view_elapsed,
        render_started.elapsed(),
        live_blocks,
        live_bytes,
    );
    result
}

async fn wait_for_paste_flush(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
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
async fn refresh_account(server: &BackendServer, state: &mut AppState) {
    if let Ok(label) = ensure_account(server).await {
        state.set_account(label);
    }
    state.set_account_plan(read_account_plan(server).await);
}

/// Plan and reset-credit entitlements for the welcome card. Fails soft: the panel
/// just shows placeholders when the server has nothing to report.
async fn read_account_plan(server: &BackendServer) -> AccountPlan {
    server
        .request("account/rateLimits/read", json!({}))
        .await
        .map(|response| AccountPlan::from_rate_limits(&response))
        .unwrap_or_default()
}

async fn read_runtime_account_plan(server: &BackendServer, model: &str) -> AccountPlan {
    if claude::is_claude_model(model) {
        AccountPlan::default()
    } else {
        read_account_plan(server).await
    }
}

/// Claude only reports usage when a turn ends, so a resumed session would show an
/// empty context on the status line. The bridge replays the stored totals instead.
fn apply_resumed_token_usage(state: &mut AppState, response: &Value) {
    let Some(usage) = response
        .get("tokenUsage")
        .filter(|value| !value.is_null())
        .cloned()
    else {
        return;
    };
    let params = json!({ "threadId": state.thread_id, "tokenUsage": usage });
    state.handle_notification("thread/tokenUsage/updated", &params);
}

fn apply_claude_account_metadata(state: &mut AppState, response: &Value) {
    if !response
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(claude::is_claude_model)
    {
        return;
    }
    let account = response.get("account").filter(|value| !value.is_null());
    let usage = response.get("usage").filter(|value| !value.is_null());
    let label = account
        .and_then(|account| account.get("email"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("Claude subscription");
    state.set_account(label.to_owned());
    state.set_account_plan(AccountPlan::from_claude(account, usage));
}

async fn ensure_account(server: &BackendServer) -> Result<String> {
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

#[allow(clippy::too_many_arguments)]
async fn start_or_resume_thread(
    server: &BackendServer,
    resume: Option<&str>,
    model: Option<&str>,
    resume_cwd: Option<&Path>,
    new_cwd: &Path,
    model_verbosity: &str,
    claude: &ClaudeSessionSettings,
    effort: Option<&str>,
    start_effort: &str,
) -> Result<Value> {
    if let Some(thread_id) = resume {
        let mut params = resume_thread_params(thread_id, claude);
        if let Some(model) = model {
            params["model"] = json!(model);
        }
        // `--effort` is the one effort that outranks the thread's own record.
        if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
            params["effort"] = json!(effort);
        }
        if let Some(cwd) = resume_cwd {
            params["cwd"] = json!(cwd.to_string_lossy());
        }
        server.request("thread/resume", params).await
    } else {
        server
            .request(
                "thread/start",
                new_thread_params(
                    new_cwd.to_string_lossy().as_ref(),
                    model,
                    None,
                    "startup",
                    model_verbosity,
                    &claude.permission_mode,
                    effort.unwrap_or(start_effort),
                ),
            )
            .await
    }
}

async fn list_sessions(
    server: &BackendServer,
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
    server: &BackendServer,
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
    value.starts_with("ses_")
        || (value.len() >= 32 && value.chars().filter(|ch| *ch == '-').count() >= 4)
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct StartupModelSelection {
    model: String,
    effort: String,
}

/// The first frame is drawn before `thread/start` answers, so it must use the
/// same root-level Codex defaults the server will use for a new thread.
fn resolve_startup_model(
    models: &[ModelInfo],
    cli_model: Option<&str>,
    cli_effort: Option<&str>,
    config: &str,
) -> Result<StartupModelSelection> {
    let configured_model = root_config_value(config, "model");
    let model = match cli_model {
        Some(requested) => choose_model(models, Some(requested))?,
        None => configured_model
            .and_then(|requested| models.iter().find(|model| model.matches_query(requested)))
            .unwrap_or(choose_model(models, None)?),
    };

    let effort = match cli_effort {
        Some(effort) => {
            validate_effort(models, &model.model, Some(effort))?;
            effort.to_owned()
        }
        None => root_config_value(config, "model_reasoning_effort")
            .filter(|effort| model.supports_effort(effort))
            .unwrap_or(&model.default_effort)
            .to_owned(),
    };

    Ok(StartupModelSelection {
        model: model.model.clone(),
        effort,
    })
}

fn should_fallback_to_claude(codex_available: bool, requested_model: Option<&str>) -> bool {
    !codex_available && requested_model.is_some_and(is_codex_model)
}

fn requested_startup_model<'a>(
    cli_model: Option<&'a str>,
    provider_config: &'a str,
) -> Option<&'a str> {
    cli_model.or_else(|| root_config_value(provider_config, "model"))
}

fn is_codex_model(model: &str) -> bool {
    !claude::is_claude_model(model) && !open_code::is_open_code_model(model)
}

fn preferred_claude_model(models: &[ModelInfo]) -> Option<&str> {
    models
        .iter()
        .filter(|model| claude::is_claude_model(&model.model))
        .find(|model| model.is_default)
        .or_else(|| {
            models
                .iter()
                .find(|model| claude::is_claude_model(&model.model))
        })
        .map(|model| model.model.as_str())
}

fn read_startup_config() -> String {
    let provider = backend::read_provider_config();
    let codex = state::codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .unwrap_or_default();
    format!("{provider}\n{codex}")
}

fn root_config_value<'a>(config: &'a str, name: &str) -> Option<&'a str> {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == name).then(|| value.trim().trim_matches(['"', '\'']))
        })
        .filter(|value| !value.is_empty())
}

fn validate_effort(models: &[ModelInfo], model_name: &str, effort: Option<&str>) -> Result<()> {
    let Some(effort) = effort.filter(|effort| !effort.is_empty()) else {
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
        let supported = if supported.is_empty() {
            "없음"
        } else {
            &supported
        };
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
    let resolved = path
        .canonicalize()
        .with_context(|| format!("작업 폴더를 열 수 없습니다: {}", path.display()))?;
    Ok(plain_windows_path(resolved))
}

/// Longest path that still works without the verbatim prefix on a machine that
/// never enabled long paths.
const MAX_PLAIN_PATH: usize = 255;

/// `canonicalize` hands back a Windows verbatim path (`\\?\C:\Source\DevezCode`),
/// and that prefix follows the folder everywhere: the welcome card, and every `cwd`
/// the runtimes echo back. Only paths short enough to survive without it lose it.
fn plain_windows_path(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    let plain = match text.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share` is `\\server\share` once the prefix comes off.
        Some(share) => format!(r"\\{share}"),
        None => match text.strip_prefix(r"\\?\") {
            Some(rest) => rest.to_owned(),
            None => return path,
        },
    };
    if plain.len() > MAX_PLAIN_PATH {
        return path;
    }
    PathBuf::from(plain)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use state::EffortInfo;
    use theme::ThemeKind;

    use super::*;

    #[test]
    fn the_resolved_folder_drops_the_windows_verbatim_prefix() {
        assert_eq!(
            plain_windows_path(PathBuf::from(r"\\?\C:\Source\DevezCode")),
            PathBuf::from(r"C:\Source\DevezCode")
        );
        assert_eq!(
            plain_windows_path(PathBuf::from(r"\\?\UNC\server\share\work")),
            PathBuf::from(r"\\server\share\work")
        );
        // Plain paths pass through, and a path that needs the prefix keeps it.
        assert_eq!(
            plain_windows_path(PathBuf::from(r"C:\Source\DevezCode")),
            PathBuf::from(r"C:\Source\DevezCode")
        );
        let long = format!(r"\\?\C:\{}", "segment\\".repeat(40));
        assert_eq!(plain_windows_path(PathBuf::from(&long)), PathBuf::from(long));
    }

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

    #[test]
    fn clipboard_image_shortcuts_accept_control_v_and_alt_v() {
        assert!(is_clipboard_image_shortcut(&press(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
        )));
        assert!(is_clipboard_image_shortcut(&press(
            KeyCode::Char('V'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        )));
        assert!(!is_clipboard_image_shortcut(&press(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn clicking_the_response_badge_cycles_response_length() {
        let mut state = starting_state();

        let action = pick_action(&mut state, Pick::ResponseLength);

        // Every display badge persists the whole vibe group, so one reading can
        // never be saved without the preset it belongs to.
        assert!(matches!(action, Action::PersistVibeDisplayModes { .. }));
        assert_eq!(state.response_length_label(), "Normal");
        state.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(state.response_length_label(), "Normal");
    }

    #[test]
    fn clicking_scroll_to_bottom_requests_the_local_scroll_action() {
        let mut state = starting_state();

        assert!(matches!(
            pick_action(&mut state, Pick::ScrollToBottom),
            Action::ScrollToBottom
        ));
    }

    #[test]
    fn clicking_a_markdown_link_requests_the_platform_handler() {
        let mut state = starting_state();

        assert!(matches!(
            pick_action(
                &mut state,
                Pick::OpenLink("file:///C:/Temp/preview.html".to_owned())
            ),
            Action::OpenUrl(ref target) if target == "file:///C:/Temp/preview.html"
        ));
    }

    #[test]
    fn clicking_shell_badge_cycles_the_global_mode() {
        let mut state = starting_state();
        let before = state.shell_display_mode();

        let action = pick_action(&mut state, Pick::ShellDisplayMode);

        assert!(matches!(action, Action::PersistVibeDisplayModes { .. }));
        assert_ne!(state.shell_display_mode(), before);
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

    fn model(name: &str, default_effort: &str, is_default: bool, efforts: &[&str]) -> ModelInfo {
        ModelInfo {
            id: name.to_owned(),
            model: name.to_owned(),
            display_name: name.to_owned(),
            efforts: efforts
                .iter()
                .map(|id| EffortInfo {
                    id: (*id).to_owned(),
                })
                .collect(),
            default_effort: default_effort.to_owned(),
            is_default,
            context_window: None,
            fast_service_tier: None,
        }
    }

    #[test]
    fn unavailable_codex_falls_back_unless_an_available_provider_was_requested() {
        assert!(!should_fallback_to_claude(false, None));
        assert!(should_fallback_to_claude(false, Some("gpt-5.6-sol")));
        assert!(!should_fallback_to_claude(false, Some("claude:sonnet")));
        assert!(!should_fallback_to_claude(false, Some("opencode:provider/model")));
        assert!(!should_fallback_to_claude(true, None));
    }

    #[test]
    fn claude_is_the_default_provider_and_uses_its_catalog_default() {
        let models = vec![
            model("gpt-5.6-sol", "high", true, &["high"]),
            model("claude:opus", "high", false, &["high"]),
            model("claude:sonnet", "high", true, &["high"]),
        ];

        assert_eq!(preferred_claude_model(&models), Some("claude:sonnet"));
    }

    #[test]
    fn configured_codex_model_enables_codex_startup_but_claude_does_not() {
        let codex = requested_startup_model(None, "model = \"gpt-5.6-sol\"\n");
        let claude = requested_startup_model(None, "model = \"claude:sonnet\"\n");
        let fresh = requested_startup_model(None, "");

        assert!(codex.is_some_and(is_codex_model));
        assert!(!claude.is_some_and(is_codex_model));
        assert!(fresh.is_none());
    }

    #[test]
    fn shifted_arrows_change_models_before_fullscreen_scrolling() {
        let mut state = AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![
                model("gpt-5.6-sol", "high", true, &["low", "high"]),
                model("gpt-5.6-terra", "high", false, &["low", "high"]),
            ],
            "gpt-5.6-sol",
            Some("high"),
        );
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);

        let action = apply_composer_inputs_with_scroll(
            &mut state,
            &mut renderer,
            vec![ComposerInput::Key(press(
                KeyCode::Down,
                KeyModifiers::SHIFT,
            ))],
        );

        assert!(matches!(action, Action::None));
        assert_eq!(state.selected_model_name(), "gpt-5.6-terra");
    }

    /// Alt+S is a view toggle, so it must open and close the panel without
    /// leaving a stray letter in the composer.
    #[test]
    fn alt_s_toggles_the_side_panel_without_editing_the_composer() {
        let mut state = AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![model("gpt-5.6-sol", "high", true, &["low", "high"])],
            "gpt-5.6-sol",
            Some("high"),
        );
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        // Goes through the paste burst buffer, which is what swallows a bare
        // printable key before the shortcut branches ever run.
        let mut paste = ComposerPasteBuffer::new();
        let now = Instant::now();

        observe_composer_key_with_scroll(
            &mut state,
            &mut renderer,
            &mut paste,
            press(KeyCode::Char('s'), KeyModifiers::ALT),
            now,
        );

        assert!(state.side_panel_open());
        assert!(state.editor.text().is_empty());

        observe_composer_key_with_scroll(
            &mut state,
            &mut renderer,
            &mut paste,
            press(KeyCode::Char('s'), KeyModifiers::ALT),
            now,
        );

        assert!(!state.side_panel_open());
        assert!(state.editor.text().is_empty());
    }

    /// The slash command is the discoverable way in, so it must toggle the same
    /// panel state the chord does.
    #[test]
    fn the_side_panel_slash_command_toggles_the_panel() {
        let mut state = AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![model("gpt-5.6-sol", "high", true, &["low", "high"])],
            "gpt-5.6-sol",
            Some("high"),
        );

        state.run_slash_command("/side-panel");
        assert!(state.side_panel_open());

        state.run_slash_command("/side-panel");
        assert!(!state.side_panel_open());
    }

    #[test]
    fn control_down_returns_a_scrolled_fullscreen_transcript_to_the_latest_row() {
        let renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);

        assert_eq!(
            scroll_request(&renderer, &press(KeyCode::Down, KeyModifiers::CONTROL)),
            Some(isize::MIN)
        );
    }

    #[test]
    fn page_down_returns_a_scrolled_fullscreen_transcript_to_the_latest_row() {
        let renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);

        assert_eq!(
            scroll_request(&renderer, &press(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(isize::MIN)
        );
    }

    #[test]
    fn startup_model_uses_the_configured_model_and_effort_before_first_draw() {
        let models = vec![
            model("gpt-5.6-sol", "low", true, &["low", "high"]),
            model("gpt-5.6-terra", "high", false, &["low", "high"]),
        ];

        let selection = resolve_startup_model(
            &models,
            None,
            None,
            "model = \"gpt-5.6-terra\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .expect("a valid Codex config should resolve");

        assert_eq!(selection.model, "gpt-5.6-terra");
        assert_eq!(selection.effort, "high");
    }

    #[test]
    fn startup_model_cli_options_override_the_configured_defaults() {
        let models = vec![
            model("gpt-5.6-sol", "low", true, &["low", "high"]),
            model("gpt-5.6-terra", "high", false, &["low", "high"]),
        ];

        let selection = resolve_startup_model(
            &models,
            Some("sol"),
            Some("high"),
            "model = \"gpt-5.6-terra\"\nmodel_reasoning_effort = \"low\"\n",
        )
        .expect("CLI values should take precedence");

        assert_eq!(selection.model, "gpt-5.6-sol");
        assert_eq!(selection.effort, "high");
    }

    #[test]
    fn startup_model_ignores_an_unsupported_configured_effort() {
        let models = vec![model("gpt-5.6-sol", "low", true, &["low"])];

        let selection =
            resolve_startup_model(&models, None, None, "model_reasoning_effort = \"xhigh\"\n")
                .expect("an obsolete config value should not prevent startup");

        assert_eq!(
            selection,
            StartupModelSelection {
                model: "gpt-5.6-sol".to_owned(),
                effort: "low".to_owned(),
            }
        );
    }

    #[test]
    fn startup_model_ignores_an_unknown_configured_model() {
        let models = vec![model("gpt-5.6-sol", "low", true, &["low"])];

        let selection = resolve_startup_model(
            &models,
            None,
            None,
            "model = \"retired-model\"\nmodel_reasoning_effort = \"low\"\n",
        )
        .expect("an obsolete configured model should fall back to the catalog default");

        assert_eq!(
            selection,
            StartupModelSelection {
                model: "gpt-5.6-sol".to_owned(),
                effort: "low".to_owned(),
            }
        );
    }

    #[test]
    fn config_value_write_params_include_the_required_merge_strategy() {
        assert_eq!(
            config_value_write_params("model", "gpt-5.6-sol"),
            json!({
                "keyPath": "model",
                "value": "gpt-5.6-sol",
                "mergeStrategy": "upsert"
            })
        );
        assert_eq!(
            config_value_write_params("shell_display_mode", "expand"),
            json!({
                "keyPath": "shell_display_mode",
                "value": "expand",
                "mergeStrategy": "upsert"
            })
        );
    }

    #[test]
    fn fresh_threads_include_the_model_selected_for_the_first_frame() {
        let params = new_thread_params(
            "C:\\repo",
            Some("gpt-5.6-terra"),
            None,
            "startup",
            "low",
            "default",
            "high",
        );

        assert_eq!(params.pointer("/developerInstructions").and_then(Value::as_str), Some(DEVEZ_INSTRUCTIONS));
        assert_eq!(
            params
                .pointer("/claudeDeveloperInstructions")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(params.pointer("/model").and_then(Value::as_str), Some("gpt-5.6-terra"));
    }

    #[test]
    fn developer_instructions_leave_browser_tool_choice_to_the_tool_list() {
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(!rules.contains("내장 브라우저 규칙"));
            assert!(!rules.contains("browser_*"));
            assert!(!rules.contains("Browser plugin"));
        }
    }

    #[test]
    fn resumed_threads_carry_the_current_rules() {
        let params = resume_thread_params("thread-1", &test_claude_settings());

        assert_eq!(params.pointer("/threadId").and_then(Value::as_str), Some("thread-1"));
        assert_eq!(
            params.pointer("/developerInstructions").and_then(Value::as_str),
            Some(DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params
                .pointer("/claudeDeveloperInstructions")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params.pointer("/initialTurnsPage/itemsView").and_then(Value::as_str),
            Some("full")
        );
    }

    fn test_claude_settings() -> ClaudeSessionSettings {
        ClaudeSessionSettings {
            model: "claude:opus".to_owned(),
            effort: "xhigh".to_owned(),
            permission_mode: "acceptEdits".to_owned(),
        }
    }

    /// The saved default rides along as a fallback only: forcing it into `model`
    /// would outrank what the resumed thread's own turns ran on.
    #[test]
    fn a_resumed_thread_carries_the_saved_defaults_as_fallbacks() {
        let params = resume_thread_params("claude:session-1", &test_claude_settings());

        assert_eq!(
            params.pointer("/claudeFallbackModel").and_then(Value::as_str),
            Some("claude:opus")
        );
        assert_eq!(
            params.pointer("/claudeFallbackEffort").and_then(Value::as_str),
            Some("xhigh")
        );
        assert_eq!(
            params.pointer("/claudePermissionMode").and_then(Value::as_str),
            Some("acceptEdits")
        );
        assert!(params.get("model").is_none());
        assert!(params.get("effort").is_none());
    }

    /// A rule written as "under Super Vibe, …" needs the preset in the request to
    /// mean anything, and nothing else in a turn carries it.
    #[test]
    fn every_turn_names_the_active_preset() {
        let notice = |vibe| {
            turn_additional_context(vibe)
                .pointer("/devez-vibe-mode/value")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .expect("the turn names its preset")
        };

        let super_vibe = notice(VibeMode::SuperVibe);
        assert!(super_vibe.contains("Super Vibe"));
        assert!(super_vibe.contains("파일 경로, 코드 블록"));
        assert!(super_vibe.contains("빌드나 테스트 명령을 넣지 않는다"));
        // Without a stated ceiling the completion report grows into five bullets.
        assert!(super_vibe.contains("세 줄 이내"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("완료 보고는 세 줄 이내로"));
        assert!(notice(VibeMode::Vibe).contains("현재 응답 모드: Vibe"));
        assert!(notice(VibeMode::Normal).contains("현재 응답 모드: Off"));
        // The English tool-call label leaks through the system prompt, so every
        // preset repeats the language rule where the turn cannot miss it.
        for vibe in [VibeMode::Vibe, VibeMode::SuperVibe, VibeMode::Normal] {
            assert!(notice(vibe).contains("영어로 시작하는 진행 문장"));
        }
        // The caps truncated the one answer that has to stay whole, so both
        // capped presets carry the exception next to the cap that broke it.
        for vibe in [VibeMode::Vibe, VibeMode::SuperVibe] {
            assert!(notice(vibe).contains("선택이나 승인을 요청할 때는 이 분량 제한을 적용하지 않는다"));
            assert!(notice(vibe).contains("AskUserQuestion 도구를 쓸 수 있으면"));
        }
    }

    #[test]
    fn every_turn_restates_the_rules() {
        let context = turn_additional_context(VibeMode::Vibe);

        assert_eq!(
            context.pointer("/devez-vibe-rules/value").and_then(Value::as_str),
            Some(DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            context.pointer("/devez-vibe-rules/kind").and_then(Value::as_str),
            Some("application")
        );
        assert_eq!(
            context
                .pointer("/claude-devez-vibe-rules/value")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("TaskCreate"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("TaskUpdate"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 응답 content block"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("모든 일반 문장은 반드시 한국어로 작성한다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("`I'll check ...`, `Fine. Building ...`"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("어떤 tool_use도 이 text보다 먼저 출력하지 않는다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("두 번째 작업 도구를 호출하면 지침 위반"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("`pending`에서 `completed`로 바로 바꾸지 않는다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("전체 200자 이내"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("완료 문구를 붙이지 않는다"));
            assert!(rules.contains("`~한 내용을 완료했습니다.`처럼 명사절을 겹쳐 쓰거나"));
            assert!(!rules.contains("`~ 내용을 완료했습니다.` 형식으로"));
        }
        // The preset caps cut the choices out of the very answer that exists to
        // present them, so each provider gets the asking form it can actually use.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("반드시 AskUserQuestion 도구로 묻는다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("선택지가 다섯 개 이상이라"));
        assert!(DEVEZ_INSTRUCTIONS.contains("선택이나 승인을 요청하는 답변에는 이 분량 제한을 적용하지 않는다"));
        assert!(DEVEZ_INSTRUCTIONS.contains("선택지를 줄이거나 문장을 도중에 끊지 않는다"));
        // Read as a per-call duty, the opening notice turned into the same
        // contentless line before every tool call.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 assistant message에만 적용한다"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("`다음 부분을 이어서 확인하겠습니다.`"));
        }
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("`Now ...` 같은 독립된 영어 진행 문장"));
        // The ban only held when it moved above the format rules and named the
        // English-word-then-Korean shape that actually leaked.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("최우선 영어 라벨 금지 규칙"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("영어 낱말 뒤에 한국어를 이어 붙이는"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 도구 호출 전에"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("파일 경로, 코드 블록"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("도구를 두 번 이상 호출할 작업"));
        assert!(DEVEZ_INSTRUCTIONS.contains("도구를 두 번 이상 호출할 작업"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("동시에 `in_progress`인 Task는 하나만"));
        assert!(DEVEZ_INSTRUCTIONS.contains("동시에 in_progress인 Task는 하나만"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("같은 내용을 반복하지 않는다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("검증하지 못한 내용은 짧게 밝힌다"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("첫 검색 결과나 단일 키워드에 의존하지 않는다"));
            assert!(rules.contains("찾지 못했다는 이유만으로 기능이나 코드가 없다고 단정하지 않는다"));
            assert!(rules.contains("현재 구현, 과거 문제의 원인, 추측을 구분"));
            assert!(rules.contains("직접적인 결론, 이를 뒷받침하는 핵심 근거"));
            assert!(rules.contains("결론 정리"));
            assert!(rules.contains("종료 직전에 여러 Task를 한꺼번에"));
        }
    }

    #[test]
    fn new_thread_params_include_selected_response_length() {
        let params = new_thread_params("C:\\repo", None, None, "startup", "low", "default", "max");

        assert_eq!(
            params.pointer("/config/model_verbosity").and_then(Value::as_str),
            Some("low")
        );
        assert_eq!(params.get("effort").and_then(Value::as_str), Some("max"));
    }

    #[test]
    fn applying_a_failed_integration_catalogue_reports_every_failure() {
        let mut state = starting_state();
        let error = apply_integrations(
            &mut state,
            IntegrationCatalog {
                skills: Err("skills offline".to_owned()),
                plugins: Err("plugins offline".to_owned()),
                apps: Err("apps offline".to_owned()),
            },
        )
        .expect_err("all three catalogue requests failed");

        assert_eq!(
            error.to_string(),
            "Skill 조회 실패: skills offline; 플러그인 조회 실패: plugins offline; App 조회 실패: apps offline"
        );
    }

    #[test]
    fn app_catalogue_uses_only_a_codex_backing_thread() {
        let backing = Some("018f3f2a-7298-7b55-9ec0-0d9bf34ac123".to_owned());

        assert_eq!(
            app_thread_id_for_model("gpt-5.6-sol", backing.clone()),
            backing
        );
        assert_eq!(
            app_thread_id_for_model("claude:sonnet", Some("codex-id".to_owned())),
            None
        );
        assert_eq!(
            app_thread_id_for_model("opencode:anthropic/claude", Some("codex-id".to_owned())),
            None
        );
    }

    #[tokio::test]
    async fn initial_integration_refresh_does_not_block_event_loop() {
        let mut receiver = Some(start_background_catalogue(async {
            std::future::pending::<IntegrationCatalog>().await
        }));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), recv_integrations(&mut receiver))
                .await
                .is_err(),
            "a pending catalogue fetch must not make the caller wait"
        );
    }

    #[tokio::test]
    async fn cost_restore_does_not_block_event_loop() {
        let mut receiver = Some(start_background_cost_restore("thread".to_owned(), || {
            std::thread::sleep(Duration::from_millis(50));
            None::<pricing::CostLedger>
        }));

        assert!(
            tokio::time::timeout(Duration::from_millis(10), recv_cost_restore(&mut receiver))
                .await
                .is_err(),
            "cost reconstruction must not make the caller wait"
        );
    }

    #[test]
    fn pasted_local_image_path_accepts_a_real_image_file() {
        let path =
            std::env::temp_dir().join(format!("devez-paste-image-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();

        let parsed = local_image_path_from_paste(&format!("{}\r\n", path.display()));

        assert_eq!(parsed.as_deref(), Some(path.as_path()));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pasted_image_text_attaches_without_entering_the_editor() {
        let path = std::env::temp_dir().join(format!(
            "devez-paste-burst-image-{}.png",
            std::process::id()
        ));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let mut state = starting_state();
        apply_composer_text(
            &mut state,
            BufferedText {
                text: path.to_string_lossy().into_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            },
        );

        assert!(state.editor.is_empty());
        assert_eq!(state.composer_image_count(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pasted_image_path_stays_in_the_focused_question_editor() {
        let path = std::env::temp_dir().join(format!(
            "devez-question-paste-image-{}.png",
            std::process::id()
        ));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [{ "label": "첫째", "description": "설명" }]
                }]
            }),
        );
        state.handle_key(press(KeyCode::Char('2'), KeyModifiers::NONE));

        apply_direct_paste(&mut state, &path.to_string_lossy());

        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text),
            Some(path.to_string_lossy().into_owned())
        );
        assert_eq!(state.composer_image_count(), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn typed_image_path_stays_text_even_when_the_file_exists() {
        let path =
            std::env::temp_dir().join(format!("devez-typed-image-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let mut state = starting_state();
        let text = path.to_string_lossy().into_owned();

        apply_composer_text(
            &mut state,
            BufferedText {
                text: text.clone(),
                pasted: false,
                target: BufferedTextTarget::Composer,
            },
        );

        assert_eq!(state.editor.text(), text);
        assert_eq!(state.composer_image_count(), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn buffered_typing_keeps_a_paste_block_collapsed_until_the_same_text_is_pasted_again() {
        let pasted = "one\ntwo\nthree\nfour\nfive\nsix";
        let mut state = starting_state();
        apply_composer_text(
            &mut state,
            BufferedText {
                text: pasted.to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            },
        );

        apply_composer_text(
            &mut state,
            BufferedText {
                text: " ".to_owned(),
                pasted: false,
                target: BufferedTextTarget::Composer,
            },
        );
        assert_eq!(state.editor.paste_summary_lines(), Some(6));

        apply_composer_text(
            &mut state,
            BufferedText {
                text: pasted.to_owned(),
                pasted: true,
                target: BufferedTextTarget::Composer,
            },
        );
        assert_eq!(state.editor.paste_summary_lines(), None);
        assert_eq!(state.editor.text(), format!("{pasted} "));
    }

    #[test]
    fn raw_second_paste_expands_without_submitting_when_ctrl_v_is_not_forwarded() {
        let pasted = "a\nb\nc\nd\ne\nf";
        let mut state = starting_state();
        state.handle_paste(pasted);
        let mut buffer = ComposerPasteBuffer::new();
        let base = Instant::now();

        for (index, ch) in pasted.chars().enumerate() {
            let at = base + Duration::from_millis(index as u64 * 40);
            if index > 0 {
                assert!(!flush_composer_paste(
                    &mut state,
                    &mut buffer,
                    at - Duration::from_millis(20),
                ));
            }
            let code = if ch == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(ch)
            };
            let action = observe_composer_key(
                &mut state,
                &mut buffer,
                press(code, KeyModifiers::NONE),
                at,
            );
            assert!(!matches!(action, Action::Submit(_)));
        }

        assert_eq!(state.editor.paste_summary_lines(), None);
        assert_eq!(state.editor.text(), pasted);
    }

    #[test]
    fn an_ime_commit_waits_until_terminal_cleanup_before_repainting_an_inline_answer() {
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" },
                        { "label": "셋째", "description": "설명" }
                    ]
                }]
            }),
        );
        state.handle_key(press(KeyCode::Char('4'), KeyModifiers::NONE));
        let mut buffer = ComposerPasteBuffer::new();

        let release = KeyEvent::new_with_kind(
            KeyCode::Char('ㅌ'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(matches!(
            observe_composer_key(&mut state, &mut buffer, release, Instant::now()),
            Action::Tick(false)
        ));
        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text)
                .as_deref(),
            Some("")
        );

        let committed_at = Instant::now();
        assert!(matches!(
            observe_composer_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Char('테'), KeyModifiers::NONE),
                committed_at,
            ),
            Action::None
        ));
        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text)
                .as_deref(),
            Some("")
        );
        assert!(buffer.is_buffering());
        assert!(!flush_composer_paste(
            &mut state,
            &mut buffer,
            committed_at + Duration::from_millis(1),
        ));
        assert!(flush_composer_paste(
            &mut state,
            &mut buffer,
            committed_at + Duration::from_millis(30),
        ));
        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text)
                .as_deref(),
            Some("테")
        );
    }

    #[test]
    fn enter_submits_a_hangul_answer_committed_in_the_same_terminal_batch() {
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" },
                        { "label": "셋째", "description": "설명" }
                    ]
                }]
            }),
        );
        state.handle_key(press(KeyCode::Char('4'), KeyModifiers::NONE));
        let mut buffer = ComposerPasteBuffer::new();
        let committed_at = Instant::now();

        assert!(matches!(
            observe_composer_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Char('답'), KeyModifiers::NONE),
                committed_at,
            ),
            Action::None
        ));
        let action = observe_composer_key(
            &mut state,
            &mut buffer,
            press(KeyCode::Enter, KeyModifiers::NONE),
            committed_at,
        );

        assert!(matches!(
            action,
            Action::RpcResponse { ref result, .. } if result.to_string().contains("답")
        ));
        let committed = state.drain_committed();
        assert_eq!(
            committed.last().map(|block| block.body.as_str()),
            Some("어느 것인가요:\n  ↳ 답")
        );
    }

    #[test]
    fn reaching_other_by_arrow_accepts_text_without_an_activation_enter() {
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" },
                        { "label": "셋째", "description": "설명" }
                    ]
                }]
            }),
        );
        for _ in 0..3 {
            state.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        }
        let mut buffer = ComposerPasteBuffer::new();
        let committed_at = Instant::now();

        observe_composer_key(
            &mut state,
            &mut buffer,
            press(KeyCode::Char('답'), KeyModifiers::NONE),
            committed_at,
        );
        assert!(flush_composer_paste(
            &mut state,
            &mut buffer,
            committed_at + Duration::from_millis(30),
        ));

        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text)
                .as_deref(),
            Some("답")
        );
    }

    #[test]
    fn choosing_number_four_focuses_only_its_inline_editor_and_enter_submits_it() {
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [
                        { "label": "첫째", "description": "설명" },
                        { "label": "둘째", "description": "설명" },
                        { "label": "셋째", "description": "설명" }
                    ]
                }]
            }),
        );
        let mut buffer = ComposerPasteBuffer::new();
        let selected_at = Instant::now();

        assert!(matches!(
            observe_composer_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Char('4'), KeyModifiers::NONE),
                selected_at,
            ),
            Action::None
        ));
        observe_composer_key(
            &mut state,
            &mut buffer,
            press(KeyCode::Char('답'), KeyModifiers::NONE),
            selected_at + Duration::from_millis(1),
        );
        assert!(flush_composer_paste(
            &mut state,
            &mut buffer,
            selected_at + Duration::from_millis(30),
        ));
        let overlay = state.view().overlay.expect("question overlay");
        assert_eq!(overlay.input.map(Editor::text).as_deref(), Some("답"));
        assert_eq!(overlay.lines[4].text, "답");
        assert!(overlay.lines[4].selected);

        assert!(matches!(
            observe_composer_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Enter, KeyModifiers::NONE),
                selected_at + Duration::from_millis(31),
            ),
            Action::RpcResponse { .. }
        ));
        assert!(state.editor.is_empty());
    }

    #[test]
    fn buffered_question_text_never_leaks_into_the_main_composer() {
        let mut state = starting_state();
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": [{ "label": "첫째", "description": "설명" }]
                }]
            }),
        );
        state.handle_key(press(KeyCode::Char('2'), KeyModifiers::NONE));
        let mut buffer = ComposerPasteBuffer::new();
        let committed_at = Instant::now();
        observe_composer_key(
            &mut state,
            &mut buffer,
            press(KeyCode::Char('직'), KeyModifiers::NONE),
            committed_at,
        );

        let action = state.click_overlay_row(1);
        assert!(matches!(action, Action::RpcResponse { .. }));
        assert!(flush_composer_paste(
            &mut state,
            &mut buffer,
            committed_at + Duration::from_millis(30),
        ));
        assert!(state.editor.is_empty());
    }

    #[test]
    fn buffered_composer_text_keeps_its_owner_when_a_question_opens() {
        let mut state = starting_state();
        let mut buffer = ComposerPasteBuffer::new();
        let typed_at = Instant::now();
        observe_composer_key(
            &mut state,
            &mut buffer,
            press(KeyCode::Char('초'), KeyModifiers::NONE),
            typed_at,
        );
        state.begin_server_request(
            json!(1),
            "item/tool/requestUserInput",
            &json!({
                "questions": [{
                    "id": "q1",
                    "question": "어느 것인가요?",
                    "options": []
                }]
            }),
        );

        assert!(flush_composer_paste(
            &mut state,
            &mut buffer,
            typed_at + Duration::from_millis(30),
        ));

        assert_eq!(state.editor.text(), "초");
        assert_eq!(
            state
                .view()
                .overlay
                .and_then(|overlay| overlay.input)
                .map(Editor::text)
                .as_deref(),
            Some("")
        );
    }

    /// A Windows clipboard holds CRLF, while the keys the terminal synthesizes
    /// for the same payload carry one Enter per line. The second paste still has
    /// to read as the block already collapsed — and its trailing line ending
    /// must not reach the composer as a submit key.
    #[test]
    fn a_crlf_second_paste_expands_the_block_instead_of_submitting() {
        let pasted = "a\r\nb\r\nc\r\nd\r\ne\r\nf";
        let mut state = starting_state();
        state.handle_paste(pasted);
        assert_eq!(state.editor.paste_summary_lines(), Some(6));
        let mut buffer = ComposerPasteBuffer::new();
        let base = Instant::now();

        let mut at = base;
        for ch in pasted.chars().filter(|&ch| ch != '\r').chain(['\n']) {
            at += Duration::from_millis(1);
            let code = if ch == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(ch)
            };
            let action =
                observe_composer_key(&mut state, &mut buffer, press(code, KeyModifiers::NONE), at);
            assert!(!matches!(action, Action::Submit(_)), "no key may submit");
        }

        assert_eq!(state.editor.paste_summary_lines(), None, "the block expanded");
        assert!(state.editor.text().starts_with(pasted));
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

    #[test]
    fn buffered_space_toggles_the_selected_statusline_field() {
        let mut state = state_with_a_model();
        state.editor.set_text("/statusline");
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        let action = observe_composer_key(
            &mut state,
            &mut ComposerPasteBuffer::new(),
            press(KeyCode::Char(' '), KeyModifiers::NONE),
            Instant::now(),
        );

        assert!(matches!(
            action,
            Action::PersistStatusLine {
                key_path: "status_line_model",
                enabled: false,
            }
        ));

        assert_eq!(
            state.view().overlay.expect("status line picker").lines[0].text,
            "☐ Model"
        );
    }

    /// The Fast badge is a direct toggle; `/fast` remains the explicit chooser.
    #[test]
    fn clicking_the_fast_badge_toggles_the_service_tier() {
        let mut state = state_with_a_model();
        state.set_fast_mode(false);

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
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Model".to_owned())
        );

        pick_action(&mut state, Pick::EffortSetting);
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Effort".to_owned())
        );

        pick_action(&mut state, Pick::Model);
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Model".to_owned())
        );
    }

    #[test]
    fn an_open_picker_still_swallows_the_response_badge() {
        let mut state = state_with_a_model();
        pick_action(&mut state, Pick::Model);

        pick_action(&mut state, Pick::ResponseLength);

        assert_eq!(state.response_length_label(), "Short");
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
        assert!(
            state
                .view()
                .status_line
                .is_some_and(|status| status.effort.as_deref() == Some("low"))
        );
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

    #[test]
    fn side_exit_key_guard_absorbs_repeats_until_the_key_settles() {
        let base = Instant::now();
        let mut guard = Some(base + SIDE_EXIT_KEY_SETTLE);
        let ctrl_c = press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let ordinary = press(KeyCode::Char('x'), KeyModifiers::NONE);

        assert!(suppress_side_exit_key(
            &mut guard,
            &ctrl_c,
            base + Duration::from_millis(200)
        ));
        assert!(suppress_side_exit_key(
            &mut guard,
            &ctrl_c,
            base + Duration::from_millis(400)
        ));
        assert!(!suppress_side_exit_key(
            &mut guard,
            &ordinary,
            base + Duration::from_millis(410)
        ));
        assert!(!suppress_side_exit_key(
            &mut guard,
            &ctrl_c,
            base + Duration::from_millis(700)
        ));
        assert_eq!(guard, None);
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
                .is_some_and(|activity| activity.contains("Working.. (")),
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
        // The first Ctrl+C interrupts and arms the quit, so its notice takes the
        // activity slot ahead of the interrupted label.
        assert!(
            state
                .view()
                .activity
                .is_some_and(|activity| activity.contains("Ctrl+C"))
        );
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

    #[test]
    fn startup_keeps_mode_clicks_until_the_thread_is_bound() {
        let mut state = starting_state();
        let mut queued = None;

        let vibe = pick_action(&mut state, Pick::VibeMode);
        assert!(hold_until_thread(&mut state, vibe, &mut queued).is_none());
        assert!(hold_until_thread(&mut state, Action::SetFast(true), &mut queued).is_none());
        assert!(hold_until_thread(
            &mut state,
            Action::SetClaudePermissionMode(state::ClaudePermissionMode::AcceptEdits),
            &mut queued,
        )
        .is_none());

        let deferred = state.take_deferred_startup_actions();
        assert_eq!(deferred.len(), 3);
        assert!(deferred.iter().any(|action| matches!(
            action,
            Action::PersistVibeDisplayModes { .. }
        )));
        assert!(deferred
            .iter()
            .any(|action| matches!(action, Action::SetFast(true))));
        assert!(deferred.iter().any(|action| matches!(
            action,
            Action::SetClaudePermissionMode(state::ClaudePermissionMode::AcceptEdits)
        )));
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

    #[test]
    fn resume_initial_turns_page_is_loaded_as_thread_history() {
        let thread = thread_with_initial_turns(&json!({
            "thread": { "id": "thread-9" },
            "initialTurnsPage": { "data": [{ "id": "turn-1", "items": [] }] }
        }))
        .expect("resume response");

        assert_eq!(thread.pointer("/turns/0/id").and_then(Value::as_str), Some("turn-1"));
    }

    #[test]
    fn resume_history_pages_request_full_items_in_chronological_order() {
        let params = turns_list_params("thread-9", Some("cursor-100"));

        assert_eq!(params.pointer("/threadId").and_then(Value::as_str), Some("thread-9"));
        assert_eq!(params.pointer("/cursor").and_then(Value::as_str), Some("cursor-100"));
        assert_eq!(params.pointer("/sortDirection").and_then(Value::as_str), Some("asc"));
        assert_eq!(params.pointer("/itemsView").and_then(Value::as_str), Some("full"));

        let first_page = turns_list_params("thread-9", None);
        assert!(first_page.get("cursor").is_none());
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
