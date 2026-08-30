mod agent;
mod app_server;
mod backend;
mod child_process;
mod claude;
mod completion;
mod devezcode;
mod editor;
mod input_log;
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
mod terminal_width;
mod theme;
mod update;

use std::{
    env, fs,
    future::Future,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use agent::AgentTurnContext;
use anyhow::{Context, Result, bail};
use app_server::ServerEvent;
use arboard::{Clipboard, ImageData};
use backend::{BackendServer, IntegrationClient};
use clap::Parser;
use completion::collect_workspace_entries;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use editor::Editor;
use futures_util::StreamExt;
use integrations::{McpServerInfo, PluginCatalog, PluginDetail};
use paste::{BufferedText, BufferedTextTarget, ComposerInput, ComposerPasteBuffer, PasteBurst};
use provider::{ProviderAuthKind, ProviderAuthRequest};
use renderer::{
    BlockKind, Pick, RenderMode, Renderer, SIDE_PANEL_INTEGRATIONS_CONNECTED, SelectionResult,
    SplitFocus, TerminalSession, View,
};
use serde_json::{Value, json};
use state::{
    AccountPlan, Action, AppState, DiffDisplayMode, LoginMethod, ModelInfo, SessionInfo,
    SessionPicker, SessionPickerResult, ShellDisplayMode, SkillProvider, VibeMode,
    load_model_context_windows,
};
use tokio::{
    sync::mpsc,
    time::{MissedTickBehavior, timeout},
};

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
    let mut server =
        BackendServer::spawn(&cli.codex, &cli.open_code, &cli.node, &cli.claude, &cwd).await?;

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
    let resume_id = resolve_startup_session(cli, server, &cwd, requested_model).await?;
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
    let (account, prefer_open_code) = if requested_claude || default_to_claude || fallback_to_claude
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
    state.push_notice(
        BlockKind::Update,
        "Tip",
        "/: Command\n@: Mentions\n$: Skills\n/provider: Set Claude Codex provider\n/side-panel: Choose side panel size\n/vibemode: Set Vibe mode\n/Response: Set Response compression type\nTab: Cycle agent role\nShift + ↑↓ model · ←→ effort\nAlt + P: Cycle side panel size",
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
        // The transcript being reopened can still carry a specialized role from
        // the process that wrote it, so the first Standard turn retires it.
        state.note_resumed_transcript();
        server.prepare_resume_runtime(resume_id).await?;
    }
    let claude = claude_session_settings(state);
    let verbosity = state.model_verbosity();
    // A fresh launch opens no session. The first prompt builds it, so the thread is
    // named after the runtime the user actually sends on: starting one here named it
    // after the launch default, and a provider picked before that first prompt had to
    // borrow the session in under the other name.
    let thread = async {
        if is_resuming {
            start_or_resume_thread(
                server,
                Some(resume_id),
                model_override,
                cli.cwd.as_ref().map(|_| cwd),
                cwd,
                verbosity,
                &claude,
                cli.effort.as_deref(),
                requested_effort,
            )
            .await
        } else {
            Ok(Value::Null)
        }
    };
    let startup = await_thread(
        server,
        state,
        renderer,
        thread,
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
    if thread_response.is_null() {
        state.set_host_loading(false);
        return run_after_startup(server, state, renderer, queued).await;
    }
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
    run_after_startup(server, state, renderer, queued).await
}

/// The screen is live and the session is either bound or still waiting for the first
/// prompt to build it. Arms the update check, sends anything typed during startup,
/// and hands over to the event loop.
async fn run_after_startup(
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    queued: Option<String>,
) -> Result<()> {
    draw(state, renderer)?;

    let (update_tx, update_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        if let Some(latest) = update::check_for_update().await {
            let _ = update_tx.send(latest).await;
        }
    });

    if let Some(text) = queued {
        draw(state, renderer)?;
        if !state.thread_pending() || open_pending_thread(server, state, renderer).await? {
            start_turn(server, state, renderer, text, None).await?;
        }
    }
    event_loop(server, state, renderer, update_rx).await
}

/// Keeps the activity row alive while a request needed to begin a turn is waiting
/// for its acknowledgement. The request itself still owns the ordering; only the
/// paint clock is allowed to advance alongside it.
async fn await_with_activity<T>(
    state: &mut AppState,
    renderer: &mut Renderer,
    request: impl Future<Output = T>,
) -> Result<T> {
    await_with_ticks(request, Duration::from_millis(80), || {
        let tick = state.render_tick();
        if !tick.redraw {
            return Ok(());
        }
        let animated = tick.animation_only && renderer.render_animation(state.animation_view())?;
        if !animated {
            draw(state, renderer)?;
        }
        Ok(())
    })
    .await
}

async fn await_with_ticks<T>(
    request: impl Future<Output = T>,
    interval: Duration,
    mut on_tick: impl FnMut() -> Result<()>,
) -> Result<T> {
    let start = tokio::time::Instant::now() + interval;
    let mut ticker = tokio::time::interval_at(start, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::pin!(request);
    loop {
        tokio::select! {
            response = &mut request => return Ok(response),
            _ = ticker.tick() => on_tick()?,
        }
    }
}

/// Builds the session the first thread-bound action needs, on the selected runtime.
/// Returns false when it could not be created, with the failure already on screen.
async fn open_pending_thread(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<bool> {
    let model = state.selected_model_name().to_owned();
    let params = new_thread_params(
        &state.cwd,
        Some(&model),
        Some(state.service_tier()),
        // Codex accepts only `startup` or `clear` here and rejects the whole
        // request otherwise, so a session opened before the first prompt reports
        // the same source a session opened by that prompt does.
        "startup",
        state.model_verbosity(),
        state.claude_permission_mode_setting().wire(),
        state.selected_effort(),
    );
    let response =
        match await_with_activity(state, renderer, server.request("thread/start", params)).await? {
            Ok(response) => response,
            Err(error) => {
                state.set_request_failed(format!("세션을 시작하지 못했습니다: {error}"));
                return Ok(false);
            }
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
    let actual_model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&model)
        .to_owned();
    let (Some(thread_id), Some(cwd)) = (thread_id, cwd) else {
        state.set_request_failed("thread/start 응답이 올바르지 않습니다.");
        return Ok(false);
    };
    let effort = response
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| state.selected_effort().to_owned());
    state.attach_thread(thread_id, cwd, &actual_model, Some(&effort));
    state.note_resume_id(&server.resume_id(&state.thread_id));
    apply_deferred_startup_actions(server, state).await;
    Ok(true)
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
                        } else if paste_clipboard_text_shortcut(
                            state,
                            &mut composer_paste,
                            &key,
                            Instant::now(),
                        ) || (is_clipboard_image_shortcut(&key) && attach_clipboard_image(state))
                        {
                            Action::None
                        } else if expand_collapsed_paste_shortcut(
                            state,
                            &mut composer_paste,
                            &key,
                            Instant::now(),
                        ) {
                            Action::Tick(true)
                        } else {
                            observe_composer_key_with_scroll(
                                state,
                                renderer,
                                &mut composer_paste,
                                key,
                                Instant::now(),
                            )
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        renderer_mouse_action(renderer, &mouse, |click| match click {
                            MouseClick::Pick(pick) => pick_action(state, pick),
                            MouseClick::Composer(index) => {
                                Action::Tick(state.move_composer_cursor(index))
                            }
                        })
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

        let force_paint = composer_paste.take_force_paint();
        redraw = force_paint
            || (!matches!(&action, Action::Tick(false)) && !composer_paste.is_buffering());
        match hold_until_thread(state, action, &mut queued) {
            // Listing sessions is the one server call the wait makes itself. It goes
            // straight to the RPC rather than through `execute_action`, which would
            // make the two mutually recursive — `/resume` waits the same way.
            Some(Action::OpenResume) => open_resume_picker(server, state).await,
            Some(Action::ShowStatus) => {
                refresh_account(server, state).await;
                state.show_status();
            }
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
        | Action::SetTheme(_)
        | Action::Copy(_)
        | Action::Cut(_)
        | Action::OpenUrl(_)) => Some(action),
        Action::ShowStatus => Some(Action::ShowStatus),
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
        action @ (Action::PersistResponseDisplayMode(_)
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
    match list_sessions(
        server,
        Some(Path::new(&state.cwd)),
        None,
        100,
        Some(state.selected_model_name()),
    )
    .await
    {
        Ok(sessions) => state.open_session_picker(sessions),
        Err(error) => state.push_notice(BlockKind::Error, "세션 목록 실패", error.to_string()),
    }
}

async fn resolve_startup_session(
    cli: &Cli,
    server: &BackendServer,
    cwd: &Path,
    requested_model: Option<&str>,
) -> Result<Option<String>> {
    let provider_model = requested_model.or(Some("claude:default"));
    if cli.continue_session {
        let sessions = list_sessions(server, Some(cwd), None, 1, provider_model).await?;
        let session = sessions
            .first()
            .context("이 작업 폴더에서 계속할 세션을 찾지 못했습니다.")?;
        return Ok(Some(session.id.clone()));
    }

    match cli.resume.as_deref() {
        None => Ok(Some(String::new())),
        Some("") => {
            let sessions = list_sessions(server, Some(cwd), None, 100, provider_model).await?;
            let mode = renderer::load_render_mode(cli.renderer.as_deref())?;
            choose_startup_session(sessions, cwd, mode).await
        }
        Some(target) => Ok(Some(
            resolve_session_target(server, target, Some(cwd), provider_model).await?,
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
                cwd: String::new(),
                plan_summary: None,
                response_collapse: None,
                fold_progress_groups: false,
                plan_active: false,
                plan_shimmer_phase: None,
                plan_effort: None,
                editor: &editor,
                composer_images: &[],
                queued_prompts: Vec::new(),
                steered_prompts: Vec::new(),
                subagents: Vec::new(),
                composer_highlights: Vec::new(),
            composer_placeholder: "",
                welcome: None,
                suggestions: Vec::new(),
                activity: None,
                activity_model: None,
                activity_phase: 0.0,
                waiting_for_response: false,
                stream_fade_tail: 0,
                activity_progress_phase: 0.0,
                footer: "Resume a Codex session".to_owned(),
                status_line: None,
                composer_notice: composer_notice.clone(),
                composer_mode: None,
                chat_layout: false,
                shell_display_mode: ShellDisplayMode::Collapse,
                diff_display_mode: DiffDisplayMode::Collapse,
                side_panel_width: None,
                side_panel_prompts_expanded: true,
                side_panel_integrations: Vec::new(),
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
                let action = renderer_mouse_action(&mut renderer, &mouse, |click| {
                    if let MouseClick::Pick(pick) = click {
                        clicked = Some(pick);
                        Action::Tick(true)
                    } else {
                        Action::Tick(false)
                    }
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

enum ManagementUpdate {
    Skill {
        provider: SkillProvider,
        name: String,
        path: String,
        source: Option<String>,
        enabled: bool,
        result: std::result::Result<Value, String>,
    },
    Plugin {
        provider: SkillProvider,
        id: String,
        name: String,
        enabled: bool,
        result: std::result::Result<Value, String>,
    },
    Mcp {
        provider: SkillProvider,
        name: String,
        enabled: bool,
        result: std::result::Result<Value, String>,
    },
    McpReconnect {
        provider: SkillProvider,
        result: std::result::Result<Value, String>,
    },
    McpLogin {
        name: String,
        claude: bool,
        result: std::result::Result<Value, String>,
    },
}

fn apply_management_update(state: &mut AppState, update: ManagementUpdate) {
    match update {
        ManagementUpdate::Skill {
            provider,
            name,
            path,
            source,
            enabled,
            result,
        } => match result {
            Ok(response) => {
                let effective = response
                    .get("effectiveEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(enabled);
                state.apply_skill_enabled(
                    provider,
                    &path,
                    source.as_deref(),
                    effective,
                    Some(format!(
                        "{name} · {}",
                        if effective { "켜짐" } else { "꺼짐" }
                    )),
                );
            }
            Err(error) => {
                let message = format!("{name} · 변경 실패: {error}");
                if !state.apply_skill_enabled(
                    provider,
                    &path,
                    source.as_deref(),
                    !enabled,
                    Some(message.clone()),
                ) {
                    state.push_notice(BlockKind::Error, "Skill 변경 실패", message);
                }
            }
        },
        ManagementUpdate::Plugin {
            provider,
            id,
            name,
            enabled,
            result,
        } => match result {
            Ok(_) => {
                state.apply_plugin_enabled(
                    provider,
                    &id,
                    enabled,
                    format!(
                        "{name} · {}{}",
                        if enabled { "켜짐" } else { "꺼짐" },
                        if provider == SkillProvider::Claude {
                            " · 새 대화부터 적용"
                        } else {
                            ""
                        }
                    ),
                );
            }
            Err(error) => {
                let message = format!("{name} · 변경 실패: {error}");
                if !state.apply_plugin_enabled(provider, &id, !enabled, message.clone()) {
                    state.push_notice(BlockKind::Error, "Plugin 변경 실패", message);
                }
            }
        },
        ManagementUpdate::Mcp {
            provider,
            name,
            enabled,
            result,
        } => match result {
            Ok(_) => {
                state.apply_mcp_enabled(
                    provider,
                    &name,
                    enabled,
                    format!("{name} · {}", if enabled { "켜짐" } else { "꺼짐" }),
                );
            }
            Err(error) => {
                let message = format!("{name} · 변경 실패: {error}");
                if !state.apply_mcp_enabled(provider, &name, !enabled, message.clone()) {
                    state.push_notice(BlockKind::Error, "MCP 변경 실패", message);
                }
            }
        },
        ManagementUpdate::McpReconnect { provider, result } => match result {
            Ok(response) => {
                state.finish_mcp_reconnect(provider, &response, "재연결했습니다.".to_owned())
            }
            Err(error) => state.push_notice(BlockKind::Error, "MCP 재연결 실패", error),
        },
        ManagementUpdate::McpLogin {
            name,
            claude,
            result,
        } => match result {
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
                } else if claude {
                    state.apply_mcp_enabled(
                        SkillProvider::Claude,
                        &name,
                        true,
                        format!("{name} · 로그인 연결을 다시 시도했습니다."),
                    );
                } else {
                    state.push_notice(
                        BlockKind::Error,
                        "MCP login 실패",
                        "authorizationUrl이 없습니다.",
                    );
                }
            }
            Err(error) => state.push_notice(BlockKind::Error, "MCP login 실패", error),
        },
    }
}

fn focused_state_mut<'a>(
    main: &'a mut AppState,
    btw: &'a mut Option<AppState>,
    focus: SplitFocus,
) -> &'a mut AppState {
    match (focus, btw.as_mut()) {
        (SplitFocus::Btw, Some(btw)) => btw,
        _ => main,
    }
}

fn event_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/turn/threadId").and_then(Value::as_str))
}

fn collapse_main_plan_for_btw(state: &mut AppState) -> Option<bool> {
    let expanded = state.plan_summary_expanded();
    state.set_plan_summary_expanded(false);
    expanded
}

fn restore_main_plan_after_btw(state: &mut AppState, expanded: &mut Option<bool>) {
    if let Some(expanded) = expanded.take() {
        state.set_plan_summary_expanded(expanded);
    }
}

fn draw_conversations(
    main: &mut AppState,
    btw: &mut Option<AppState>,
    focus: SplitFocus,
    renderer: &mut Renderer,
) -> Result<()> {
    let Some(btw) = btw.as_mut() else {
        return draw(main, renderer);
    };
    devezcode::sync(
        main.host_session_id(),
        main.busy,
        main.compacting(),
        main.host_loading(),
        main.awaiting_input(),
    );
    let main_discarded = main.take_discarded_prompt_ids();
    let btw_discarded = btw.take_discarded_prompt_ids();
    renderer.remove_history_blocks(&main_discarded)?;
    renderer.remove_split_history_blocks(&btw_discarded);
    let main_committed = main.drain_committed();
    let btw_committed = btw.drain_committed();
    let result = renderer.render_split(
        &main_committed,
        main.view(),
        &btw_committed,
        btw.view(),
        focus,
    );
    if result.is_ok() {
        main.note_response_frame_rendered(&main_committed);
        btw.note_response_frame_rendered(&btw_committed);
    }
    result
}

async fn start_split_turn(
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
    let agent_context = state.next_agent_context();
    let mut params = json!({
        "threadId": state.thread_id,
        "input": input,
        "model": model,
        "serviceTier": state.service_tier(),
        "permissions": state.permission_profile(),
        "additionalContext": turn_additional_context(state.vibe_mode(), agent_context)
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
    if let Err(error) = server.request("turn/start", params).await {
        state.set_request_failed(error.to_string());
    } else {
        state.note_agent_dispatch_succeeded(agent_context);
    }
}

async fn open_btw(
    server: &mut BackendServer,
    main: &mut AppState,
    prompt: Option<String>,
) -> Option<AppState> {
    let response = server
        .request(
            "thread/fork",
            json!({
                "threadId": main.thread_id,
                "model": main.selected_model_name(),
                "effort": main.selected_effort(),
                "claudeDeveloperInstructions": CLAUDE_DEVEZ_INSTRUCTIONS,
                "serviceTier": main.service_tier(),
                "ephemeral": true,
                "threadSource": "devez-vibe"
            }),
        )
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            main.push_notice(BlockKind::Error, "BTW 시작 실패", error.to_string());
            return None;
        }
    };
    let Some(thread_id) = response
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        main.push_notice(
            BlockKind::Error,
            "BTW 시작 실패",
            "thread/fork 응답에 thread ID가 없습니다.",
        );
        return None;
    };
    let cwd = response
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(&main.cwd)
        .to_owned();
    let model = main.selected_model_name().to_owned();
    let effort = main.selected_effort().to_owned();
    let mut btw = main.forked_side_state(thread_id, cwd, &model, Some(&effort));
    if let Some(prompt) = prompt {
        btw.begin_side_prompt(prompt.clone());
        start_split_turn(server, &mut btw, prompt, None).await;
    }
    Some(btw)
}

async fn close_btw(
    server: &mut BackendServer,
    btw: &mut Option<AppState>,
    renderer: &mut Renderer,
) {
    let Some(btw) = btw.take() else {
        return;
    };
    if let Some(turn_id) = btw.turn_id {
        let _ = server
            .request(
                "turn/interrupt",
                json!({ "threadId": btw.thread_id, "turnId": turn_id }),
            )
            .await;
    }
    let _ = server
        .request("thread/unsubscribe", json!({ "threadId": btw.thread_id }))
        .await;
    renderer.end_split();
}

async fn execute_split_conversation_action(
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    management_tx: &mpsc::UnboundedSender<ManagementUpdate>,
    action: Action,
    provider_handoff: Option<Value>,
) -> Result<bool> {
    match action {
        Action::Submit(text) => {
            renderer.scroll_to_bottom();
            start_split_turn(server, state, text, provider_handoff).await;
            Ok(false)
        }
        Action::Steer(text) => {
            let Some(turn_id) = state.turn_id.clone() else {
                state.set_request_failed("활성 turn ID가 없어 추가 입력을 보낼 수 없습니다.");
                return Ok(false);
            };
            devezcode::note_prompt(&text);
            let input = state.turn_input(text);
            if let Err(error) = server
                .request(
                    "turn/steer",
                    json!({
                        "threadId": state.thread_id,
                        "expectedTurnId": turn_id,
                        "input": input
                    }),
                )
                .await
            {
                state.push_notice(BlockKind::Error, "추가 입력 실패", error.to_string());
            }
            Ok(false)
        }
        Action::Interrupt => {
            if let Some(turn_id) = state.turn_id.clone()
                && let Err(error) = server
                    .request(
                        "turn/interrupt",
                        json!({ "threadId": state.thread_id, "turnId": turn_id }),
                    )
                    .await
            {
                state.push_notice(BlockKind::Error, "중단 실패", error.to_string());
            }
            Ok(false)
        }
        action => execute_action(server, state, renderer, management_tx, action).await,
    }
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
    // Streamed text is revealed here rather than when a delta lands, so the pace
    // follows a clock of its own instead of the provider's arrival jitter.
    let _timer_resolution = TimerResolution::raise();
    let mut stream_tick = tokio::time::interval(STREAM_FRAME);
    stream_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_stream_reveal = Instant::now();
    let mut resize = ResizeTracker::new();
    let (workspace_tx, mut workspace_rx) = mpsc::channel(1);
    let (management_tx, mut management_rx) = mpsc::unbounded_channel();
    let mut cost_restore_rx = None;
    let mut indexed_cwd = None;
    let mut integration_key = None;
    let mut integration_rx = None;
    let mut skills_key = None;
    let mut skills_rx = None;
    let mut side_exit_key_guard = None;
    let mut btw_state = None;
    let mut btw_parent_plan_expanded = None;
    let mut split_focus = SplitFocus::Btw;
    draw(state, renderer)?;

    loop {
        if let Some(thread_id) = state.take_cost_restore() {
            cost_restore_rx = Some(start_cost_restore(thread_id));
        }
        let subagent_probes = state.take_codex_subagent_probes();
        if !subagent_probes.is_empty() {
            let mut changed = false;
            for (id, running) in codex_subagent_statuses(server, subagent_probes).await {
                changed |= state.resolve_codex_subagent_probe(&id, running);
            }
            if changed {
                draw_conversations(state, &mut btw_state, split_focus, renderer)?;
            }
        }
        // A quiet turn is asked about rather than assumed dead. Only a runtime that
        // answers "not running" ends the wait, so a long think is never cut short.
        if let Some(turn_id) = state.take_stall_probe()
            && !turn_is_running(server, &state.thread_id, &turn_id).await
            && state.resolve_stall_probe(&turn_id)
        {
            let action = state
                .take_queued_prompt()
                .map(|text| state.start_queued_prompt(text))
                .unwrap_or(Action::None);
            let handoff = matches!(&action, Action::Submit(_))
                .then(|| provider_handoff_snapshot(state, renderer));
            if btw_state.is_some() {
                if execute_split_conversation_action(
                    server,
                    state,
                    renderer,
                    &management_tx,
                    action,
                    handoff,
                )
                .await?
                {
                    break;
                }
            } else if execute_action(server, state, renderer, &management_tx, action).await? {
                break;
            }
            draw_conversations(state, &mut btw_state, split_focus, renderer)?;
        }
        // A session picked while the previous one was still starting is switched to
        // here. The event loop is the only place that can drive it: the wait it was
        // requested from cannot resume out of itself without recursing into another
        // wait, whereas this loop just comes back around.
        if let Some(deferred) = state.take_deferred_resume() {
            close_btw(server, &mut btw_state, renderer).await;
            restore_main_plan_after_btw(state, &mut btw_parent_plan_expanded);
            split_focus = SplitFocus::Main;
            let should_quit = execute_action(
                server,
                state,
                renderer,
                &management_tx,
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
                send_queued_prompt(server, state, renderer, text).await?;
            }
            draw(state, renderer)?;
            continue;
        }
        if SIDE_PANEL_INTEGRATIONS_CONNECTED {
            let current_integration_key = (
                state.thread_id.clone(),
                state.cwd.clone(),
                state.selected_model_name().to_owned(),
            );
            if integration_key.as_ref() != Some(&current_integration_key) {
                integration_key = Some(current_integration_key);
                integration_rx = Some(start_integration_refresh(server, state));
            }
        }
        // The side panel stays disconnected, but `@` and `$` completion still
        // need the selected provider's own skill list on every cwd/model change.
        let current_skills_key = (state.cwd.clone(), state.selected_model_name().to_owned());
        if skills_key.as_ref() != Some(&current_skills_key) {
            skills_key = Some(current_skills_key);
            skills_rx = Some(start_skills_refresh(server, state).await);
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
        // Set when an edit took a selection. Paste batching holds the screen still
        // while keys arrive in a burst, but text vanishing from the prompt has to
        // be seen at once — otherwise the selection looks untouched until the next
        // unrelated event flushes the batch.
        let mut selection_edited = false;
        let mut action_focus = split_focus;
        let paste_deadline = composer_paste.flush_deadline();
        let action = tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        if input_log::enabled() {
                            let selection = renderer.composer_selection_range();
                            let select_all = renderer.composer_select_all_active();
                            let text = focused_state_mut(state, &mut btw_state, split_focus)
                                .editor
                                .text();
                            input_log::record(|| {
                                format!(
                                    "key {:?} mods={:?} kind={:?} select_all={select_all} \
                                     selection={selection:?} composer={text:?}",
                                    key.code, key.modifiers, key.kind
                                )
                            });
                        }
                        if btw_state.is_some()
                            && key.code == KeyCode::Tab
                            && key.modifiers.is_empty()
                            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        {
                            let input_state = focused_state_mut(state, &mut btw_state, split_focus);
                            flush_composer_paste(input_state, &mut composer_paste, Instant::now());
                            split_focus = split_focus.toggled();
                            renderer.clear_selection();
                            Action::Tick(true)
                        } else {
                            let input_state =
                                focused_state_mut(state, &mut btw_state, split_focus);
                            if suppress_side_exit_key(
                                &mut side_exit_key_guard,
                                &key,
                                Instant::now(),
                            ) {
                                input_state.disarm_quit();
                                renderer.clear_selection();
                                Action::None
                            } else if paste_clipboard_text_shortcut(
                                input_state,
                                &mut composer_paste,
                                &key,
                                Instant::now(),
                            ) || (is_clipboard_image_shortcut(&key)
                                && attach_clipboard_image(input_state))
                            {
                                renderer.clear_selection();
                                Action::None
                            } else if expand_collapsed_paste_shortcut(
                                input_state,
                                &mut composer_paste,
                                &key,
                                Instant::now(),
                            ) {
                                renderer.clear_selection();
                                Action::Tick(true)
                            } else if is_selection_delete_key(&key)
                                && let Some(range) = composer_replace_range(renderer, input_state)
                                && input_state.delete_composer_selection(range)
                            {
                                // The drag selected composer text, so the key takes the
                                // selection rather than the character at the cursor.
                                renderer.clear_selection();
                                selection_edited = true;
                                Action::Tick(true)
                            } else if is_cut_shortcut(&key)
                                && let Some(range) = composer_replace_range(renderer, input_state)
                                && let Some(text) = input_state.composer_text_in(range.clone())
                                && input_state.delete_composer_selection(range)
                            {
                                // The text reaches the clipboard first, so a cut that
                                // the delete somehow refuses leaves the prompt intact.
                                renderer.clear_selection();
                                selection_edited = true;
                                Action::Cut(text)
                            } else if is_selection_replace_key(&key)
                                && let Some(range) = composer_replace_range(renderer, input_state)
                                && input_state.delete_composer_selection(range)
                            {
                                // Typing over a selection replaces it, so the character
                                // is inserted where the selected text used to be. Text
                                // an IME commits arrives as these same keys, so a
                                // syllable typed over the selection replaces it too.
                                input_log::record(|| {
                                    format!("  replaced selection for {:?}", key.code)
                                });
                                renderer.clear_selection();
                                selection_edited = true;
                                let action = observe_composer_key_with_scroll(
                                    input_state,
                                    renderer,
                                    &mut composer_paste,
                                    key,
                                    Instant::now(),
                                );
                                // The batch would otherwise hold this character
                                // until the next event, leaving an empty prompt on
                                // screen where the replacement should already be.
                                flush_composer_paste_now(input_state, &mut composer_paste);
                                match action {
                                    Action::Tick(false) => Action::Tick(true),
                                    action => action,
                                }
                            } else if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                if let Some(text) = renderer.selected_text() {
                                    // This Ctrl+C is a copy, so it neither arms nor
                                    // spends the quit.
                                    input_state.disarm_quit();
                                    renderer.clear_selection();
                                    Action::Copy(text)
                                } else {
                                    // Typing means the drag is over and its highlight is
                                    // stale, so it goes before the key is acted on. A
                                    // release is the tail of a press that was already
                                    // acted on, and terminals that report it must not
                                    // undo the selection that press just made.
                                    let cleared = key.kind != KeyEventKind::Release
                                        && renderer.clear_selection();
                                    let action = observe_composer_key_with_scroll(
                                        input_state,
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
                                // stale, so it goes before the key is acted on. A
                                // release is the tail of a press that was already acted
                                // on, and terminals that report it must not undo the
                                // selection that press just made.
                                let cleared =
                                    key.kind != KeyEventKind::Release && renderer.clear_selection();
                                let action = observe_composer_key_with_scroll(
                                    input_state,
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
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        let input_state = focused_state_mut(state, &mut btw_state, split_focus);
                        // Clicking or scrolling is input as well, so a Ctrl+C armed
                        // before it must not be spent by the Ctrl+C after it.
                        input_state.disarm_quit();
                        renderer_mouse_action(renderer, &mouse, |click| match click {
                            MouseClick::Pick(pick) => pick_action(input_state, pick),
                            MouseClick::Composer(index) => {
                                Action::Tick(input_state.move_composer_cursor(index))
                            }
                        })
                    }
                    Some(Ok(Event::Paste(text))) => {
                        let input_state = focused_state_mut(state, &mut btw_state, split_focus);
                        if input_log::enabled() {
                            let select_all = renderer.composer_select_all_active();
                            let selection = renderer.composer_selection_range();
                            let composer = input_state.editor.text();
                            input_log::record(|| {
                                format!(
                                    "paste {text:?} select_all={select_all} \
                                     selection={selection:?} composer={composer:?}"
                                )
                            });
                        }
                        // A paste replaces the selection the way typing does. Text an
                        // IME commits can reach the composer this way too, and it is
                        // the replacement the user typed, so it takes the same path.
                        if let Some(range) = composer_replace_range(renderer, input_state) {
                            input_state.delete_composer_selection(range);
                            selection_edited = true;
                            input_log::record(|| "  replaced selection for paste".to_owned());
                        }
                        renderer.clear_selection();
                        if composer_paste.take_discarded_paste(&text) {
                            // The shortcut already expanded the block this
                            // payload stands for.
                            Action::Tick(true)
                        } else {
                            flush_composer_paste(
                                input_state,
                                &mut composer_paste,
                                Instant::now(),
                            );
                            if let Some(action) = input_state.paste_as_prompt_answer(&text) {
                                action
                            } else {
                                if !attach_clipboard_image(input_state) {
                                    apply_direct_paste(input_state, &text);
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
                        let target_is_btw = btw_state.as_ref().is_some_and(|btw| {
                            event_thread_id(&params) == Some(btw.thread_id.as_str())
                        });
                        action_focus = if target_is_btw {
                            SplitFocus::Btw
                        } else {
                            SplitFocus::Main
                        };
                        let target = focused_state_mut(state, &mut btw_state, action_focus);
                        target.handle_notification(&method, &params);
                        if method == "turn/completed" && !target_is_btw {
                            server.persist_provider_handoff(
                                &target.thread_id,
                                provider_handoff_snapshot(target, renderer),
                            );
                        }
                        if matches!(
                            method.as_str(),
                            "mcpServer/oauthLogin/completed" | "mcpServer/startupStatus/updated"
                        ) {
                            integration_key = None;
                        }
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
                            && target.take_pending_interrupt().is_some();
                        if target.take_account_refresh() {
                            refresh_account(server, target).await;
                        }
                        if interrupt_after_start {
                            Action::Interrupt
                        } else if method == "turn/completed"
                            // A runtime that compacts without running a turn ends
                            // the wait here, so the queue drains from here too.
                            || (method == "thread/compacted" && !target.host_turn_busy())
                        {
                            target
                                .take_queued_prompt()
                                .map(|text| target.start_queued_prompt(text))
                                .unwrap_or(Action::None)
                        } else if method == "skills/changed" {
                            Action::RefreshSkills
                        } else if is_paced_text_delta(&method) {
                            // The frame tick paints this; drawing on arrival would
                            // put the provider's cadence back on screen.
                            Action::Tick(false)
                        } else {
                            Action::None
                        }
                    }
                    Some(ServerEvent::Request { id, method, params }) => {
                        let target_is_btw = btw_state.as_ref().is_some_and(|btw| {
                            event_thread_id(&params) == Some(btw.thread_id.as_str())
                        });
                        action_focus = if target_is_btw {
                            SplitFocus::Btw
                        } else {
                            SplitFocus::Main
                        };
                        focused_state_mut(state, &mut btw_state, action_focus)
                            .begin_server_request(id, &method, &params)
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
            Some((model, result)) = recv_skills(&mut skills_rx) => {
                if let Ok(response) = result {
                    state.update_skills_for_model(&model, &response);
                }
                Action::None
            }
            Some((cwd, entries)) = workspace_rx.recv() => {
                if state.cwd == cwd {
                    state.update_workspace_entries(entries);
                }
                Action::None
            }
            Some(update) = management_rx.recv() => {
                apply_management_update(state, update);
                Action::None
            }
            _ = wait_for_paste_flush(paste_deadline), if paste_deadline.is_some() => {
                let input_state = focused_state_mut(state, &mut btw_state, split_focus);
                Action::Tick(flush_composer_paste(input_state, &mut composer_paste, Instant::now()))
            }
            _ = stream_tick.tick() => {
                // Measured, not assumed: a repaint can overrun the interval and the
                // runtime drops the ticks it missed, so the reveal is sized by the
                // time that actually passed.
                let now = Instant::now();
                let elapsed = now.duration_since(last_stream_reveal);
                last_stream_reveal = now;
                let main_reveal = state.drain_stream_text(elapsed);
                let btw_reveal = btw_state
                    .as_mut()
                    .map(|btw| btw.drain_stream_text(elapsed));
                perf::record_reveal(elapsed, main_reveal.clusters, main_reveal.backlog);
                let revealed = main_reveal.changed()
                    || state.response_collapse_animating()
                    || btw_reveal.as_ref().is_some_and(|reveal| reveal.changed())
                    || btw_state
                        .as_ref()
                        .is_some_and(AppState::response_collapse_animating);
                if revealed {
                    animation_tick = false;
                }
                Action::Tick(revealed)
            }
            _ = activity_tick.tick() => {
                if let Some(action) = state.take_expired_user_input_response() {
                    action_focus = SplitFocus::Main;
                    action
                } else if let Some(action) = btw_state
                    .as_mut()
                    .and_then(AppState::take_expired_user_input_response)
                {
                    action_focus = SplitFocus::Btw;
                    action
                } else {
                    let main_tick = state.render_tick();
                    let btw_tick = btw_state.as_mut().map(AppState::render_tick);
                    let mut redraw = main_tick.redraw
                        || btw_tick.as_ref().is_some_and(|tick| tick.redraw);
                    animation_tick = btw_state.is_none() && main_tick.animation_only;
                    if renderer.recover_external_screen_write() {
                        redraw = true;
                        animation_tick = false;
                    }
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
            }
        };

        // Windows exposes a paste as many key events. Do not render between
        // those events while the composer is collecting them; rendering is
        // much slower than parsing and used to make long pastes crawl.
        let force_paint = composer_paste.take_force_paint();
        let redraw = selection_edited
            || force_paint
            || (!matches!(&action, Action::Tick(false)) && !composer_paste.is_buffering());
        let returning_from_side = matches!(&action, Action::ReturnFromSide);
        let split_handoff = (btw_state.is_some()
            && action_focus == SplitFocus::Main
            && matches!(&action, Action::Submit(_)))
        .then(|| provider_handoff_snapshot(state, renderer));
        let mut select_all_pending = false;
        let should_quit = match action {
            // Claimed ahead of the split routing: the highlight belongs to the
            // renderer, which paints whichever composer the key came from. It is
            // taken after the paint, when the prompt rows the highlight indexes
            // are the ones on screen.
            Action::SelectComposerAll => {
                select_all_pending = true;
                false
            }
            Action::StartSide(prompt) if btw_state.is_none() => {
                if let Some(btw) = open_btw(server, state, prompt).await {
                    btw_parent_plan_expanded = collapse_main_plan_for_btw(state);
                    btw_state = Some(btw);
                    split_focus = SplitFocus::Btw;
                }
                false
            }
            Action::StartSide(_) => {
                split_focus = SplitFocus::Btw;
                false
            }
            Action::ReturnFromSide if btw_state.is_some() => {
                close_btw(server, &mut btw_state, renderer).await;
                state.settle_response_collapse();
                restore_main_plan_after_btw(state, &mut btw_parent_plan_expanded);
                split_focus = SplitFocus::Main;
                false
            }
            action if btw_state.is_some() => {
                let target = focused_state_mut(state, &mut btw_state, action_focus);
                execute_split_conversation_action(
                    server,
                    target,
                    renderer,
                    &management_tx,
                    action,
                    split_handoff,
                )
                .await?
            }
            action => execute_action(server, state, renderer, &management_tx, action).await?,
        };
        if returning_from_side {
            side_exit_key_guard = Some(Instant::now() + SIDE_EXIT_KEY_SETTLE);
        }
        if redraw {
            let animation_started = Instant::now();
            let animated = btw_state.is_none()
                && animation_tick
                && renderer.render_animation(state.animation_view())?;
            if animated {
                perf::record_animation(animation_started.elapsed());
            }
            if !animated {
                draw_conversations(state, &mut btw_state, split_focus, renderer)?;
            }
        }
        // The paint above is what tells the renderer where the prompt characters
        // ended up, so the highlight is taken from it and painted by one more.
        if select_all_pending {
            let selected = renderer.select_composer_all();
            input_log::record(|| {
                format!(
                    "  select all -> {selected} selection={:?}",
                    renderer.composer_selection_range()
                )
            });
            if selected {
                draw_conversations(state, &mut btw_state, split_focus, renderer)?;
            }
        }
        // Sent after the highlight settles for this pass, so the host's copy of
        // the flag matches what is on screen before the next key reaches it.
        renderer.report_composer_selection()?;
        if should_quit || connection_closed {
            break;
        }
    }
    Ok(())
}

const WHEEL_ROWS: isize = 3;
const SIDE_EXIT_KEY_SETTLE: Duration = Duration::from_millis(250);
/// How often held text is checked for its next reveal. Assistant text arrives at
/// roughly fifty characters a second, well under one per screen refresh, so what
/// the eye reads as stutter is the spacing between single characters rather than
/// the refresh rate. A tick well below one frame lets that spacing land where the
/// pace asks for it instead of on the nearest refresh boundary.
const STREAM_FRAME: Duration = Duration::from_millis(4);

/// Windows resolves timers to the system tick — 15.6ms by default — so a 16ms
/// request alternates between one tick and two, and the reveal it drives jitters
/// between 16ms and 31ms. Raising the resolution for as long as the app runs is
/// what makes a sub-frame tick mean anything.
struct TimerResolution;

impl TimerResolution {
    fn raise() -> Self {
        #[cfg(windows)]
        // SAFETY: `timeBeginPeriod` takes a millisecond count and only changes
        // this process's timer resolution. It is paired with `timeEndPeriod` in
        // `Drop`, as the API requires.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: Undoes the matching `timeBeginPeriod(1)` above.
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

fn is_paced_text_delta(method: &str) -> bool {
    matches!(
        method,
        "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/plan/delta"
    )
}

fn is_side_exit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Key-repeat records can remain queued while the parent thread is being
/// resumed. Keep extending the guard while they arrive so a held close key can
/// never become an interrupt or quit on the parent screen.
fn suppress_side_exit_key(guard: &mut Option<Instant>, key: &KeyEvent, now: Instant) -> bool {
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
    Scroll(isize, u16, u16),
    SelectionStart(u16, u16),
    SelectionUpdate(u16, u16),
    SelectionEnd(u16, u16),
    CancelSelection,
    Hover(u16, u16),
    None,
}

enum MouseClick {
    Pick(Pick),
    Composer(usize),
}

/// Shift is the terminal's own escape hatch: holding it while dragging bypasses
/// mouse reporting in every terminal worth naming, so those events are left
/// alone and the user still gets native selection when they want it.
fn mouse_request(mouse: &MouseEvent) -> MouseRequest {
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        return match mouse.kind {
            MouseEventKind::ScrollUp => MouseRequest::Scroll(WHEEL_ROWS, mouse.column, mouse.row),
            MouseEventKind::ScrollDown => {
                MouseRequest::Scroll(-WHEEL_ROWS, mouse.column, mouse.row)
            }
            MouseEventKind::Down(MouseButton::Left)
            | MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Left) => MouseRequest::CancelSelection,
            _ => MouseRequest::None,
        };
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => MouseRequest::Scroll(WHEEL_ROWS, mouse.column, mouse.row),
        MouseEventKind::ScrollDown => MouseRequest::Scroll(-WHEEL_ROWS, mouse.column, mouse.row),
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

/// `on_click` says what a click means to the caller: chrome and overlay picks,
/// or a character boundary in the main composer.
fn renderer_mouse_action(
    renderer: &mut Renderer,
    mouse: &MouseEvent,
    on_click: impl FnOnce(MouseClick) -> Action,
) -> Action {
    let request = mouse_request(mouse);
    // Some embedded terminals deliver the press but swallow the matching
    // release.  Chrome controls must not depend on that release: activate a
    // known pick as soon as it is pressed, while plain text keeps the normal
    // drag-to-select path below. A prompt's disclosure covers the prompt's own
    // text, which the user drags across to copy, so that one waits for the
    // release and only fires when the press turned out to be a plain click.
    if let MouseRequest::SelectionStart(column, row) = request
        && let Some(pick) = renderer.pick_at(column, row)
        && !matches!(pick, Pick::History(_))
    {
        let cleared = renderer.clear_selection();
        return match on_click(MouseClick::Pick(pick)) {
            Action::Tick(changed) => Action::Tick(changed || cleared),
            action => action,
        };
    }

    match request {
        MouseRequest::Scroll(delta, _, row) => Action::Tick(renderer.scroll_at(row, delta)),
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
                    Some(Pick::History(group_id)) => {
                        renderer.toggle_tool(group_id);
                        Action::Tick(true)
                    }
                    Some(pick) => match on_click(MouseClick::Pick(pick)) {
                        Action::Tick(_) => Action::Tick(true),
                        action => action,
                    },
                    None => {
                        if let Some(index) = renderer.composer_cursor_position(column, row) {
                            renderer.clear_selection();
                            return match on_click(MouseClick::Composer(index)) {
                                Action::Tick(_) => Action::Tick(true),
                                action => action,
                            };
                        }
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
            state.open_vibe_mode_picker();
            Action::None
        }
        Pick::ResponseDisplayMode => {
            state.open_response_display_picker();
            Action::None
        }
        Pick::FastMode => state.run_command("/fast"),
        Pick::ShellDisplayMode => {
            state.cycle_shell_display_mode();
            Action::PersistVibeDisplayModes {
                vibe: state.vibe_mode(),
                response: state.response_length(),
                shell: state.shell_display_mode(),
                diff: state.diff_display_mode(),
            }
        }
        Pick::DiffDisplayMode => {
            state.cycle_diff_display_mode();
            Action::PersistVibeDisplayModes {
                vibe: state.vibe_mode(),
                response: state.response_length(),
                shell: state.shell_display_mode(),
                diff: state.diff_display_mode(),
            }
        }
        Pick::PlanSummary => {
            state.toggle_plan_summary();
            Action::Tick(true)
        }
        Pick::PromptSection => {
            state.toggle_side_panel_prompts();
            Action::Tick(true)
        }
        Pick::McpSection(provider) => {
            state.toggle_side_panel_mcp(&provider);
            Action::Tick(true)
        }
        Pick::PluginSection(provider) => {
            state.toggle_side_panel_plugins(&provider);
            Action::Tick(true)
        }
        Pick::RemoveQueuedPrompt(index) => {
            state.remove_queued_prompt(index);
            Action::Tick(true)
        }
        Pick::OpenLink(target) => Action::OpenUrl(target),
        Pick::AgentMode => state.click_agent_mode(),
        Pick::Model => state.run_command("/model"),
        Pick::EffortSetting => state.run_command("/effort"),
        Pick::Subagent(index) => state.open_subagent(index),
        Pick::ScrollToBottom => Action::ScrollToBottom,
        Pick::History(_) => Action::None,
        Pick::Prompt(block_id) => Action::ScrollToPrompt(block_id),
        Pick::Close => state.close_overlay(),
        Pick::Row(index) => state.click_overlay_row(index),
        Pick::Effort(step) => state.click_effort_step(step),
    }
}

/// Maps a key to a transcript scroll, or `None` to let the session have it.
/// Page keys move one viewport at a time while Ctrl+Down keeps the explicit
/// shortcut to the latest row.
fn scroll_request(renderer: &Renderer, key: &KeyEvent) -> Option<isize> {
    if renderer.mode() != RenderMode::Fullscreen {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down {
        return Some(isize::MIN);
    }
    match key.code {
        KeyCode::PageUp => Some(renderer.page_rows()),
        KeyCode::PageDown => Some(-renderer.page_rows()),
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

async fn interrupt_turn(server: &mut BackendServer, state: &mut AppState) {
    let Some(turn_id) = state.turn_id.clone() else {
        return;
    };
    let params = json!({ "threadId": state.thread_id, "turnId": turn_id });
    if let Err(error) = server.request("turn/interrupt", params).await {
        state.push_notice(BlockKind::Error, "중단 실패", error.to_string());
    }
}

async fn execute_action(
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    management_tx: &mpsc::UnboundedSender<ManagementUpdate>,
    action: Action,
) -> Result<bool> {
    match action {
        action @ (Action::None
        | Action::Tick(_)
        | Action::Copy(_)
        | Action::Cut(_)
        | Action::OpenUrl(_)
        | Action::SetTheme(_)
        | Action::ScrollToBottom
        | Action::ScrollToPrompt(_)
        | Action::SelectComposerAll
        | Action::Quit) => return execute_local_action(state, renderer, action),
        Action::ShowStatus => {
            refresh_account(server, state).await;
            state.show_status();
        }
        Action::Submit(text) => {
            renderer.scroll_to_bottom();
            let handoff = provider_handoff_snapshot(state, renderer);
            // The prompt and its waiting state reach the screen before the request
            // is awaited. A turn that has to build a runtime session first — the
            // first prompt after a provider switch — would otherwise hold the frame
            // for the whole round trip and read as a stall in the composer.
            draw(state, renderer)?;
            // A session that does not exist yet is built here, by the prompt that
            // needs it, so it is named after the runtime this prompt runs on.
            if state.thread_pending() && !open_pending_thread(server, state, renderer).await? {
                return Ok(false);
            }
            start_turn(server, state, renderer, text, Some(handoff)).await?
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
        Action::Interrupt => interrupt_turn(server, state).await,
        // Cancelling a question the runtime is blocked on takes both halves: the
        // request has to be answered so the bridge stops waiting, and the turn it
        // belongs to has to stop, or the tool would simply carry on unanswered.
        Action::CancelUserInput { id, interrupt } => {
            // Codex types this reply as `{answers}`, so the cancel marker rides
            // alongside an empty answer set rather than replacing it.
            if let Err(error) = server.respond(id, json!({ "answers": {}, "cancelled": true })) {
                state.push_notice(BlockKind::Error, "응답 전송 실패", error.to_string());
            }
            if interrupt {
                interrupt_turn(server, state).await;
            }
        }
        Action::NewThread => return start_new_thread(server, state, renderer).await,
        Action::OpenResume => open_resume_picker(server, state).await,
        Action::ResumeThread(target) => {
            return resume_thread(server, state, renderer, &target).await;
        }
        Action::ActivateCodex => activate_codex(server, state).await,
        Action::ActivateOpenCode => {
            if open_code::has_connected_provider() {
                activate_open_code(server, state).await;
            } else {
                open_provider_connection(server, state, renderer).await?;
            }
        }
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
            // Unlike a prompt, a Fast choice can be the first thread-bound action
            // after launch or `/new`. Create the selected provider's session first
            // so `thread/settings/update` never receives an empty thread id.
            if state.thread_pending() {
                state.set_host_loading(true);
                draw(state, renderer)?;
                let opened = open_pending_thread(server, state, renderer).await;
                state.set_host_loading(false);
                if !opened? {
                    return Ok(false);
                }
            }
            set_fast_mode(server, state, enabled).await;
        }
        Action::OpenClaudePermissions(notice) => match server
            .request("claude/permissions/status", json!({ "cwd": state.cwd }))
            .await
        {
            Ok(status) => state.open_claude_permissions(&status, notice),
            Err(error) => {
                state.push_notice(BlockKind::Error, "Claude 권한 조회 실패", error.to_string())
            }
        },
        Action::UpdateClaudePermission {
            action,
            behavior,
            value,
            destination,
        } => match server
            .request(
                "claude/permissions/update",
                json!({
                    "cwd": state.cwd,
                    "action": action,
                    "behavior": behavior,
                    "value": value,
                    "destination": destination,
                }),
            )
            .await
        {
            Ok(status) => state.open_claude_permissions(
                &status,
                Some(
                    if action == "add" {
                        "Claude 권한 규칙을 추가했습니다."
                    } else {
                        "Claude 권한 규칙을 제거했습니다."
                    }
                    .to_owned(),
                ),
            ),
            Err(error) => state.push_notice(
                BlockKind::Error,
                "Claude 권한 규칙 변경 실패",
                error.to_string(),
            ),
        },
        Action::RetryClaudePermissionDenial { tool, input } => {
            renderer.scroll_to_bottom();
            if let Err(error) = server
                .request(
                    "claude/permissions/retry",
                    json!({
                        "threadId": state.thread_id,
                        "tool": tool,
                        "input": input,
                    }),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Error,
                    "Claude 권한 재시도 실패",
                    error.to_string(),
                );
            }
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
            state.persist_session_modes();
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
            state.persist_session_modes();
        }
        Action::PersistSidePanelDefault(stage) => {
            if let Err(error) = server
                .request(
                    "config/value/write",
                    config_value_write_params("side_panel_stage", stage.config_value()),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Warning,
                    "사이드패널 기본값 저장 실패",
                    error.to_string(),
                );
            }
        }
        Action::PersistResponseDisplayMode(mode) => {
            if let Err(error) = server
                .request(
                    "config/value/write",
                    config_value_write_params("response_display_mode", mode.config_value()),
                )
                .await
            {
                state.push_notice(
                    BlockKind::Warning,
                    "Response 표시 설정 저장 실패",
                    error.to_string(),
                );
            }
            state.persist_session_modes();
        }
        Action::PersistVibeDisplayModes {
            vibe,
            response,
            shell,
            diff,
        } => {
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
                    // The fork inherits the parent's effort, so an answer that
                    // omits it must not drop the pane back to the model default.
                    let effort = response
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .filter(|effort| !effort.is_empty())
                        .unwrap_or_else(|| state.selected_effort())
                        .to_owned();
                    let effort = (!effort.is_empty()).then_some(effort);
                    if let Some(thread_id) = thread_id {
                        renderer.clear_screen()?;
                        state.enter_side_thread(thread_id, cwd, &model, effort.as_deref());
                        if let Some(prompt) = prompt {
                            state.begin_side_prompt(prompt.clone());
                            start_turn(server, state, renderer, prompt, None).await?;
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
        Action::OpenMcp(notice) => match list_mcp_servers(server, state).await {
            Ok(response) => {
                let model = state.selected_model_name().to_owned();
                state.update_mcp_servers_for_model(&response, &model);
                state.open_mcp_picker(McpServerInfo::list_from_value(&response), notice);
            }
            Err(error) => {
                let model = state.selected_model_name().to_owned();
                state.note_mcp_query_error_for_model(error.to_string(), &model);
                state.push_notice(BlockKind::Error, "MCP 목록 실패", error.to_string());
            }
        },
        Action::ReconnectMcp(name) => {
            let provider = SkillProvider::from_model(state.selected_model_name());
            let client = server.integration_client(provider.model_hint());
            let thread_id = integration_mcp_thread_id(server, state);
            let (Some(client), Some(thread_id)) = (client, thread_id) else {
                state.push_notice(
                    BlockKind::Error,
                    "MCP 재연결 실패",
                    "현재 provider의 MCP 세션이 아직 시작되지 않았습니다.",
                );
                return Ok(false);
            };
            let sender = management_tx.clone();
            tokio::spawn(async move {
                let reconnect = match provider {
                    SkillProvider::Claude => {
                        client
                            .request(
                                "mcp/reconnect",
                                json!({ "sessionId": thread_id.clone(), "name": name }),
                            )
                            .await
                    }
                    SkillProvider::Codex => {
                        client.request("config/mcpServer/reload", json!({})).await
                    }
                };
                let result = match reconnect {
                    Ok(_) => client
                        .request(
                            "mcpServerStatus/list",
                            json!({
                                "threadId": thread_id,
                                "detail": "toolsAndAuthOnly",
                                "limit": 100
                            }),
                        )
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let _ = sender.send(ManagementUpdate::McpReconnect { provider, result });
            });
        }
        Action::SetMcpEnabled {
            provider,
            name,
            enabled,
        } => {
            let Some(client) = server.integration_client(provider.model_hint()) else {
                state.apply_mcp_enabled(
                    provider,
                    &name,
                    !enabled,
                    format!("{name} · 변경 실패: provider가 연결되지 않았습니다."),
                );
                return Ok(false);
            };
            let (method, params) = mcp_toggle_request(provider, &state.thread_id, &name, enabled);
            let sender = management_tx.clone();
            tokio::spawn(async move {
                let mut result = client
                    .request(method, params)
                    .await
                    .map_err(|error| error.to_string());
                if provider == SkillProvider::Codex && result.is_ok() {
                    result = client
                        .request("config/mcpServer/reload", json!({}))
                        .await
                        .map_err(|error| error.to_string());
                }
                let _ = sender.send(ManagementUpdate::Mcp {
                    provider,
                    name,
                    enabled,
                    result,
                });
            });
        }
        Action::ConnectProvider => open_provider_connection(server, state, renderer).await?,
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
                                    .complete_provider_oauth(&provider_id, method_index, None)
                                    .await
                                {
                                    Ok(()) => {
                                        refresh_provider_models(server, state, &provider_name)
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
            let model = state.selected_model_name().to_owned();
            let claude = claude::is_claude_model(&model);
            let Some(client) = server.integration_client(&model) else {
                state.push_notice(
                    BlockKind::Error,
                    "MCP login 실패",
                    "현재 provider가 연결되지 않았습니다.",
                );
                return Ok(false);
            };
            let (method, params) = if claude {
                (
                    "mcp/login",
                    json!({ "sessionId": state.thread_id, "name": name.clone() }),
                )
            } else {
                (
                    "mcpServer/oauth/login",
                    json!({
                        "name": name.clone(),
                        "threadId": state.thread_id,
                        "timeoutSecs": 300
                    }),
                )
            };
            let sender = management_tx.clone();
            tokio::spawn(async move {
                let result = client
                    .request(method, params)
                    .await
                    .map_err(|error| error.to_string());
                let _ = sender.send(ManagementUpdate::McpLogin {
                    name,
                    claude,
                    result,
                });
            });
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
        Action::OpenPlugins { scope, notice } => match list_plugins(server, state).await {
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
                integration_request(server, state, "plugin/read", params),
                list_plugins(server, state)
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
        Action::PreparePluginInstall(query) => match list_plugins(server, state).await {
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
        Action::PreparePluginUninstall(query) => match list_plugins(server, state).await {
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
        Action::SetPlugin { query, enabled } => match list_plugins(server, state).await {
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
                    if let Err(error) = write_plugin_enabled(
                        server,
                        state,
                        &plugin.id,
                        plugin.scope.as_deref(),
                        enabled,
                    )
                    .await
                    {
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
                                "{} · {}{}",
                                plugin.display_name,
                                if enabled { "enabled" } else { "disabled" },
                                if claude::is_claude_model(state.selected_model_name()) {
                                    "\n새 대화부터 적용됩니다."
                                } else {
                                    ""
                                }
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
            let model = state.selected_model_name().to_owned();
            let provider = SkillProvider::from_model(&model);
            let (method, params) =
                plugin_write_request(&model, &plugin.id, plugin.scope.as_deref(), enabled);
            match server.integration_client(&model) {
                Some(client) => {
                    let sender = management_tx.clone();
                    let id = plugin.id;
                    let name = plugin.display_name;
                    tokio::spawn(async move {
                        let result = client
                            .request(method, params)
                            .await
                            .map_err(|error| error.to_string());
                        let _ = sender.send(ManagementUpdate::Plugin {
                            provider,
                            id,
                            name,
                            enabled,
                            result,
                        });
                    });
                }
                None => {
                    state.apply_plugin_enabled(
                        provider,
                        &plugin.id,
                        !enabled,
                        format!(
                            "{} · 변경 실패: provider가 연결되지 않았습니다.",
                            plugin.display_name
                        ),
                    );
                }
            }
        }
        Action::OpenMarketplaces(notice) => match list_plugins(server, state).await {
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
            match integration_request(server, state, "marketplace/add", params).await {
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
            match integration_request(
                server,
                state,
                "marketplace/remove",
                json!({ "marketplaceName": name }),
            )
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
            let using_claude = claude::is_claude_model(state.selected_model_name());
            let reconnect = if using_claude {
                integration_request(server, state, "plugin/reload", json!({})).await
            } else {
                server.request("config/mcpServer/reload", json!({})).await
            };
            let mcp_response = if reconnect.is_ok() {
                match list_mcp_servers(server, state).await {
                    Ok(response) => Some(response),
                    Err(error) => {
                        let model = state.selected_model_name().to_owned();
                        state.note_mcp_query_error_for_model(error.to_string(), &model);
                        None
                    }
                }
            } else {
                None
            };
            if let Some(response) = &mcp_response {
                let model = state.selected_model_name().to_owned();
                state.update_mcp_servers_for_model(response, &model);
            }
            let servers = (!using_claude)
                .then(|| mcp_response.as_ref().map(McpServerInfo::list_from_value))
                .flatten();
            let mut report = format_reload_report(
                integrations.as_ref().err().map(ToString::to_string),
                reconnect.as_ref().err().map(ToString::to_string),
                servers.as_deref(),
            );
            if using_claude
                && let Ok(response) = &reconnect
                && let Some(message) = response.get("message").and_then(Value::as_str)
            {
                if !report.is_empty() {
                    report.push('\n');
                }
                report.push_str(message);
            }
            state.push_notice(
                if integrations.is_err() || reconnect.is_err() {
                    BlockKind::Warning
                } else {
                    BlockKind::System
                },
                if using_claude {
                    "✓ Plugin settings refreshed"
                } else {
                    "✓ Plugins reloaded"
                },
                report,
            );
        }
        Action::UpgradeMarketplaces => {
            match integration_request(server, state, "marketplace/upgrade", json!({})).await {
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
            let using_claude = claude::is_claude_model(state.selected_model_name());
            let response = integration_request(
                server,
                state,
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
                    let base = if using_claude {
                        "설치했습니다. 새 대화부터 Skill과 도구가 적용됩니다."
                    } else {
                        "Skill과 멘션은 바로 사용할 수 있습니다.\n\
                         MCP 서버가 포함된 플러그인이면 /reload-plugins로 적용하세요."
                    };
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
            let using_claude = claude::is_claude_model(state.selected_model_name());
            match integration_request(
                server,
                state,
                "plugin/uninstall",
                json!({ "pluginId": target.plugin_id }),
            )
            .await
            {
                Ok(_) => {
                    state.push_notice(
                        BlockKind::System,
                        "✓ Plugin uninstalled",
                        if using_claude {
                            format!("{} · 새 대화부터 제외됩니다.", target.display_name)
                        } else {
                            format!(
                                "{} · Skill과 멘션은 즉시 사라집니다.\n\
                                 MCP 도구가 있었다면 /reload-plugins로 정리하세요.",
                                target.display_name
                            )
                        },
                    );
                    let _ = refresh_integrations(server, state, true).await;
                }
                Err(error) => {
                    state.push_notice(BlockKind::Error, "플러그인 제거 실패", error.to_string())
                }
            }
        }
        Action::ShowSkills => {
            let provider = SkillProvider::from_model(state.selected_model_name());
            open_skills(server, state, provider, None).await;
        }
        Action::OpenSkills { provider, notice } => {
            open_skills(server, state, provider, notice).await;
        }
        Action::SetSkill {
            provider,
            name,
            enabled,
        } => match list_skills(server, &state.cwd, false, provider).await {
            Ok(skills) => match resolve_skill(&skills, &name) {
                Some(skill) if skill.enabled == enabled => {
                    open_skills(
                        server,
                        state,
                        provider,
                        Some(format!(
                            "{} · 이미 {}",
                            skill.name,
                            if enabled { "켜짐" } else { "꺼짐" }
                        )),
                    )
                    .await
                }
                Some(skill) => match write_skill_enabled(
                    server,
                    &state.cwd,
                    provider,
                    &skill.path,
                    skill.source.as_deref(),
                    &skill.scope,
                    enabled,
                )
                .await
                {
                    Ok(response) => {
                        let effective = response
                            .get("effectiveEnabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(enabled);
                        open_skills(
                            server,
                            state,
                            provider,
                            Some(format!(
                                "{} · {}",
                                skill.name,
                                if effective { "켜짐" } else { "꺼짐" }
                            )),
                        )
                        .await;
                    }
                    Err(error) => {
                        open_skills(
                            server,
                            state,
                            provider,
                            Some(format!("{} · 변경 실패: {error}", skill.name)),
                        )
                        .await;
                    }
                },
                None => {
                    open_skills(
                        server,
                        state,
                        provider,
                        Some(format!("{name} · Skill을 찾을 수 없습니다.")),
                    )
                    .await;
                }
            },
            Err(error) => state.push_notice(BlockKind::Error, "Skill 조회 실패", error.to_string()),
        },
        Action::SetSkillEnabled {
            provider,
            name,
            path,
            source,
            scope,
            enabled,
        } => {
            let request = skill_write_request(
                &state.cwd,
                provider,
                &path,
                source.as_deref(),
                &scope,
                enabled,
            );
            let client = server.integration_client(provider.model_hint());
            match (request, client) {
                (Ok((method, params)), Some(client)) => {
                    let sender = management_tx.clone();
                    tokio::spawn(async move {
                        let result = client
                            .request(method, params)
                            .await
                            .map_err(|error| error.to_string());
                        let _ = sender.send(ManagementUpdate::Skill {
                            provider,
                            name,
                            path,
                            source,
                            enabled,
                            result,
                        });
                    });
                }
                (Err(error), _) => {
                    state.apply_skill_enabled(
                        provider,
                        &path,
                        source.as_deref(),
                        !enabled,
                        Some(format!("{name} · 변경 실패: {error}")),
                    );
                }
                (_, None) => {
                    state.apply_skill_enabled(
                        provider,
                        &path,
                        source.as_deref(),
                        !enabled,
                        Some(format!(
                            "{name} · 변경 실패: provider가 연결되지 않았습니다."
                        )),
                    );
                }
            }
        }
        Action::RefreshSkills => {
            let provider = SkillProvider::from_model(state.selected_model_name());
            match list_skills(server, &state.cwd, true, provider).await {
                Ok(response) => state.update_skills_for_provider(provider, &response),
                Err(error) => {
                    state.push_notice(BlockKind::Warning, "Skill 새로고침 실패", error.to_string())
                }
            }
        }
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
        }
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
            .request(
                "model/list",
                json!({ "includeHidden": false, "limit": 100 }),
            )
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

async fn activate_open_code(server: &mut BackendServer, state: &mut AppState) {
    match server.start_open_code().await {
        Ok(()) => match server
            .request(
                "model/list",
                json!({ "includeHidden": false, "limit": 100 }),
            )
            .await
        {
            Ok(response) => {
                state.replace_models(parse_models(&response));
                state.switch_to_open_code();
                // 전환 직후 헤더가 이전 런타임의 계정·플랜을 들고 있지 않게 한다.
                refresh_account(server, state).await;
            }
            Err(error) => state.push_notice(
                BlockKind::Error,
                "OpenCode 모델 조회 실패",
                error.to_string(),
            ),
        },
        Err(error) => state.push_notice(BlockKind::Error, "OpenCode 사용 불가", error.to_string()),
    }
}

async fn open_provider_connection(
    server: &mut BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<()> {
    state.open_provider_loading();
    draw(state, renderer)?;
    match server.provider_catalog().await {
        Ok(catalog) => state.open_provider_picker(&catalog),
        Err(error) => state.provider_connection_failed(error.to_string()),
    }
    Ok(())
}

/// Clears the screen back to the welcome panel without opening a session. The next
/// prompt opens one, so the new conversation is named after the runtime it actually
/// runs on. Returns `true` when the user quits, which this path never does.
async fn start_new_thread(
    _server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
) -> Result<bool> {
    renderer.clear_screen()?;
    state.prepare_new_thread();
    state.begin_thread_switch();
    // Nothing is started here: the next prompt builds the session, on whichever
    // runtime is selected by then. Until it does, the screen is an empty new
    // conversation with no session behind it.
    draw(state, renderer)?;
    Ok(false)
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
    let thread_id = match resolve_session_target(
        server,
        target,
        Some(Path::new(&current_cwd)),
        Some(state.selected_model_name()),
    )
    .await
    {
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

/// Clears to a named loading screen while `thread_id` and its complete history
/// load. Shared by `/resume` and the return from a side conversation.
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
        request_resume_thread(server, resume_thread_params(thread_id, &claude)),
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

/// Tail shared by `/new` and `/resume`: finish deferred settings, replace the
/// loading screen with the bound session, then send whatever was typed during
/// the wait.
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
        send_queued_prompt(server, state, renderer, text).await?;
    }
    Ok(false)
}

/// Commits mode clicks made while a new session had no id yet. Their local value
/// was already visible and will be used by any queued first prompt; this only
/// synchronizes the new thread once it is addressable.
async fn apply_deferred_startup_actions(server: &BackendServer, state: &mut AppState) {
    for action in state.take_deferred_startup_actions() {
        match action {
            Action::SetFast(enabled) => set_fast_mode(server, state, enabled).await,
            Action::PersistResponseDisplayMode(mode) => {
                if let Err(error) = server
                    .request(
                        "config/value/write",
                        config_value_write_params("response_display_mode", mode.config_value()),
                    )
                    .await
                {
                    state.push_notice(
                        BlockKind::Warning,
                        "Response 표시 설정 저장 실패",
                        error.to_string(),
                    );
                }
                state.persist_session_modes();
            }
            Action::PersistVibeDisplayModes {
                vibe,
                response,
                shell,
                diff,
            } => persist_vibe_display_modes(server, state, vibe, response, shell, diff).await,
            _ => unreachable!("only startup-safe mode actions are deferred"),
        }
    }
}

/// Sends a prompt typed during a switch. Returning from a side conversation can
/// bring a turn back with it, so the prompt joins that turn rather than starting a
/// competing one.
async fn send_queued_prompt(
    server: &BackendServer,
    state: &mut AppState,
    renderer: &mut Renderer,
    text: String,
) -> Result<()> {
    let Some(turn_id) = state.turn_id.clone() else {
        return start_turn(server, state, renderer, text, None).await;
    };
    devezcode::note_prompt(&text);
    let input = state.turn_input(text);
    let params = json!({
        "threadId": state.thread_id,
        "expectedTurnId": turn_id,
        "input": input
    });
    if let Err(error) =
        await_with_activity(state, renderer, server.request("turn/steer", params)).await?
    {
        state.push_notice(BlockKind::Error, "추가 입력 실패", error.to_string());
    }
    Ok(())
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
            .request(
                "thread/turns/list",
                turns_list_params(&thread_id, cursor.as_deref()),
            )
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
        // The composer already gave the text up; only the clipboard is left. A
        // failure says so rather than pretending the cut landed somewhere.
        Action::Cut(text) => {
            match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(&text)) {
                Ok(()) => state.set_cut_notice(),
                Err(error) => {
                    state.push_notice(BlockKind::Error, "잘라내기 실패", error.to_string())
                }
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
        Action::ScrollToBottom => {
            renderer.scroll_to_bottom();
        }
        Action::ScrollToPrompt(block_id) => {
            renderer.scroll_to_prompt(block_id);
        }
        Action::SelectComposerAll => {
            renderer.select_composer_all();
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
    let Some(params) = fast_settings_update_params(&state.thread_id, &service_tier) else {
        state.push_notice(
            BlockKind::Error,
            "Fast 전환 실패",
            "세션을 먼저 시작하지 못했습니다.",
        );
        return;
    };
    let update = server.request("thread/settings/update", params).await;
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
                state.push_notice(BlockKind::Warning, "Fast 설정 저장 실패", error.to_string());
            }
        }
        Err(error) => state.push_notice(BlockKind::Error, "Fast 전환 실패", error.to_string()),
    }
}

fn fast_settings_update_params(thread_id: &str, service_tier: &str) -> Option<Value> {
    (!thread_id.is_empty()).then(|| {
        json!({
            "threadId": thread_id,
            "serviceTier": service_tier
        })
    })
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
            state.push_notice(
                BlockKind::Warning,
                "Vibe 표시 설정 저장 실패",
                error.to_string(),
            );
            break;
        }
    }
    state.persist_session_modes();
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
    "사용자가 요청했거나 원인, 영향, 변경 범위, 실행 방법을 정확히 판단하는 데 꼭 필요한 경우가 아니면 ",
    "클래스명, 메서드명, 변수명 등 기술 식별자, 파일 경로, 명령어와 코드 조각을 답변에 쓰지 않는다. ",
    "필요한 경우에도 사용자 판단에 필요한 최소 범위만 쓴다.\n",
    "답변 형식 규칙:\n",
    "- 서론, 인사, 맺음말 요약을 쓰지 않고 결론부터 쓴다.\n",
    "- 이모티콘과 이모지를 쓰지 않는다. 답변, 진행 안내, Task 제목, 커밋 메시지 어디에도 넣지 않는다.\n",
    "- 응답 모드와 관계없이 최종 답변은 가능한 한 불릿 두세 개, 전체 200자 내외로 쓰며 불릿 하나에 두 문장을 넘기지 않는다. 사용자가 자세한 설명을 요청할 때만 늘린다.\n",
    "- 다만 사용자에게 선택이나 승인을 요청하는 답변에는 이 분량 제한을 적용하지 않는다. ",
    "고를 수 있는 선택지, 각 선택지의 결과, 판단에 필요한 사실을 하나도 빠뜨리지 않고 적고, ",
    "분량을 맞추려고 선택지를 줄이거나 문장을 도중에 끊지 않는다. ",
    "마지막 줄에서 무엇을 선택하면 되는지 한 문장으로 묻는다.\n",
    "- 산문 문단 대신 불릿과 코드 블록을 쓴다.\n",
    "- 코드 변경 보고에서도 파일 경로와 핵심 코드는 사용자 판단에 꼭 필요한 경우에만 최소한으로 보여주고, 요청받지 않은 해설을 덧붙이지 않는다.\n",
    "- 계획이나 작업 단계를 답변 본문에 다시 나열하지 않는다.\n",
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
    "- 조사나 수정 결과를 보고할 때는 확인된 원인, 사용자에게 미치는 영향, 실제 조치를 짧게 함께 적는다. 원인을 확인하지 못했으면 추측으로 메우지 말고 미확인이라고 밝힌다. `수정했습니다`, `확인했습니다`만으로 결과를 끝내지 않는다.\n",
    "- 결론과 완료 보고는 바꾼 대상과 결과를 구체적으로 지목해 쓴다. `일부 수정했습니다`, `관련 부분을 개선했습니다`처럼 대상이 드러나지 않는 문장으로 얼버무리지 않는다.\n",
    "- 재개 기록, 사용자 질문, 권한 응답처럼 외부 상태를 기다리는 경우에는 실제 응답이나 오류를 받기 전 취소·거절·완료·원인을 단정하지 않는다. 질문 도구가 전달되지 않거나 응답을 받지 못했다는 오류가 오면 필요한 질문을 일반 text로 다시 보여 주고, 답이 필요한 작업은 사용자가 답하기 전 파일을 바꾸지 않는다.\n",
    "- Skill 적용, 지침 확인, 내부 도구 호출 같은 내부 절차를 사용자에게 commentary로 알리지 않는다. ",
    "사용자 판단에 필요한 진행 상황이나 결과만 알린다.\n",
    "진행 보고 규칙:\n",
    "- 진행 안내와 답변에는 `진행 안내:`, `결론:`, `완료 보고:` 같은 라벨이나 머리글을 붙이지 않고 문장으로 바로 시작한다. 규칙 속 용어는 지시일 뿐 그대로 출력할 문구가 아니다.\n",
    "- 첫 진행 안내를 낸 뒤에는 새 사실이 사용자 판단을 바꾸거나 작업 범위가 달라질 때만 짧게 알리고, 같은 내용을 반복하지 않는다.\n",
    "- 무엇을 알아냈는지 담기지 않은 진행 문장은 쓰지 않는다. ",
    "`다음 부분을 이어서 확인하겠습니다.`, `이어서 진행하겠습니다.`, `계속 확인하겠습니다.`처럼 ",
    "다음에 무엇을 왜 보는지 없는 문장은 같은 응답에서 한 번도 쓰지 않는다.\n",
    "계획 규칙:\n",
    "- 실행 단계가 두 개 이상이거나 도구를 두 번 이상 호출할 작업, 설계 판단이 필요한 작업에서는 첫 작업 도구 호출 전에 반드시 `update_plan`을 호출해 짧은 계획을 먼저 세운다. 진행 안내 문장, 조사 항목 나열, 답변 본문의 불릿은 `update_plan`을 대신하지 않는다.\n",
    "- 단순 질문, 단 한 번의 고립된 조회, 한 줄 수정처럼 도구 한 번으로 끝난다고 확신할 수 있는 요청에만 계획을 만들지 않는다. 한 번으로 끝날지 확신할 수 없으면 반드시 계획부터 만든다. 첫 작업 도구를 호출한 뒤 두 번째 도구 앞에서 계획을 만드는 것은 지침 위반이다.\n",
    "- `update_plan`의 각 step에는 반드시 제목 자체의 맨 앞에 순서대로 `1. `, `2. `, `3. ` 번호를 넣는다. 화면의 상태 기호나 목록 서식에 번호 표시를 맡기지 않으며, 새 계획은 항상 `1. `부터 시작한다.\n",
    "- Task에는 실제 조사·수정·검증 작업만 넣고, 결론 정리나 완료 보고만을 별도 Task로 만들지 않는다.\n",
    "- 종료 직전에 여러 Task를 한꺼번에 completed로 바꾸지 않는다. 각 Task의 첫 작업 도구를 호출하기 전에 해당 Task를 in_progress로 바꾸고, 그 작업이 끝난 직후 completed로 바꾼다.\n",
);

/// Claude Code already owns its native task system. These rules preserve the
/// same visible workflow while naming the Claude tools it can actually call.
const CLAUDE_DEVEZ_INSTRUCTIONS: &str = concat!(
    "Devez Vibe에서 작업한다. Task 목록의 설명과 모든 Task 제목은 반드시 자연스러운 한국어로 작성한다. ",
    "코드, 명령어, 경로, 제품명 등 기술 식별자는 원문을 유지한다.\n",
    "사용자가 요청했거나 원인, 영향, 변경 범위, 실행 방법을 정확히 판단하는 데 꼭 필요한 경우가 아니면 ",
    "클래스명, 메서드명, 변수명 등 기술 식별자, 파일 경로, 명령어와 코드 조각을 답변에 쓰지 않는다. ",
    "필요한 경우에도 사용자 판단에 필요한 최소 범위만 쓴다.\n",
    "최우선 분량 규칙: 응답 모드와 관계없이 최종 답변은 불릿 두세 개, 전체 200자 내외로 쓰고 불릿 하나에 두 문장을 넘기지 않는다. ",
    "넘치면 문장을 다듬지 말고 덜 중요한 불릿을 통째로 지운다. 다른 규칙과 충돌하면 분량이 이긴다. ",
    "정확성·보고 규칙은 이미 쓴 문장을 정확하게 만들라는 뜻이지 문장을 더 쓰라는 뜻이 아니다. ",
    "사용자가 자세한 설명을 요청했거나 선택지를 나열할 때만 이 상한을 푼다. ",
    "답변을 출력하기 직전에 불릿 수와 글자 수를 세고 넘치면 지운 뒤 출력한다.\n",
    "최우선 한국어 전용 규칙: 사용자에게 보이는 text는 한 글자도 빠짐없이 한국어 문장으로만 이루어진다. ",
    "진행 안내, 도구 호출 앞뒤 라벨, 중간 보고, 최종 답변이 모두 여기에 해당하며, ",
    "모든 일반 문장은 반드시 한국어로 작성한다. 사용자가 영어로 요청해도 응답 언어는 한국어로 유지한다. ",
    "영어는 코드, 명령어, 경로, 제품명 등 기술 식별자와 사용자가 그대로 인용한 문자열에만 허용하고, 그 밖의 낱말은 하나도 영어로 두지 않는다. ",
    "한 문장 안에서 영어 절과 한국어 절을 섞지 않는다. 반복해서 새는 위반이 둘 있으므로 출력 전에 반드시 걸러낸다. ",
    "첫째, 영어 낱말로 문장을 시작한 뒤 한국어를 이어 붙이는 형태이며, 주로 영어 부사·접속사로 문장을 시작하는 형태로 샌다. ",
    "사용자에게 보이는 모든 text는 첫 글자가 한글 음절이어야 한다. 예: `First 토글 함수를 넣습니다.` → `토글 함수를 넣습니다.` ",
    "둘째, 도구 결과에 대한 판정을 `Confirmed ... works.`, `Good, that closes correctly.`, `Done.`처럼 영어로 적고 뒤에 한국어를 잇는 형태다. ",
    "확인 결과는 `확인했습니다.`, `문제없습니다.`처럼 한국어로 적는다. ",
    "text를 출력하기 직전에 모든 문장을 훑어 기술 식별자가 아닌 영어가 있으면 한국어로 바꾼 뒤 출력한다.\n",
    "최우선 시작 응답 규칙: 단순 질문이 아닌 작업에서는 첫 응답 content block을 반드시 사용자에게 보이는 짧은 진행 안내 text로 출력한다. ",
    "TaskCreate를 포함한 어떤 tool_use도 이 text보다 먼저 출력하지 않는다. 같은 assistant message에 text와 tool_use를 함께 출력할 때도 text를 앞에 둔다. ",
    "진행 안내에는 요청의 구체 대상과 바로 수행할 조사·수정 동작을 한두 문장으로 적는다. ",
    "`요청 내용을 확인하고 필요한 작업을 진행하겠습니다.`처럼 대상·근거·행동이 없는 포괄적 접수 문구는 쓰지 않는다. ",
    "진행 안내와 답변에는 `진행 안내:`, `결론:`, `완료 보고:` 같은 라벨이나 머리글을 붙이지 않고 문장으로 바로 시작한다. 규칙 속 용어는 지시일 뿐 그대로 출력할 문구가 아니다. ",
    "이 규칙은 사용자 메시지에 대한 첫 assistant message에만 적용한다. ",
    "그다음부터는 알릴 새 사실이 없으면 tool_use 앞에 text를 붙이지 않고 도구를 바로 호출한다.\n",
    "최우선 작업 단계 규칙: 실행 단계가 두 개 이상이거나 도구를 두 번 이상 호출할 작업, 설계 판단이 필요한 작업에서는 ",
    "첫 작업 도구 호출 전에 Claude Code의 TaskCreate로 짧은 작업 목록을 만든다. 진행 안내 text, 조사 항목 나열, 답변 본문의 불릿은 TaskCreate를 대신하지 않는다. ",
    "도구 한 번으로 끝난다고 확신할 수 있는 요청에만 Task를 만들지 않고, 확신할 수 없으면 반드시 TaskCreate부터 호출한다. ",
    "TaskCreate 없이 첫 작업 도구를 호출한 뒤 두 번째 작업 도구를 호출하거나, 두 번째 도구 앞에서 뒤늦게 TaskCreate를 호출하면 지침 위반이다. ",
    "모든 TaskCreate의 subject에는 반드시 제목 자체의 맨 앞에 순서대로 `1. `, `2. `, `3. ` 번호를 넣고, 화면의 상태 기호나 목록 서식에 번호 표시를 맡기지 않는다. 번호는 새 작업 목록마다 항상 `1. `부터 다시 시작한다. ",
    "TaskList에 이미 끝난 Task가 남아 있어도 그 번호를 이어받지 않는다. ",
    "Task에는 실제 조사·수정·검증 작업만 넣고, `결론 정리`, `결과 보고`, `완료 보고`만을 별도 Task로 만들지 않는다. ",
    "동시에 `in_progress`인 Task는 하나만 두고, 현재 Task를 `completed`로 바꾼 뒤 다음 Task를 `in_progress`로 바꾸고 해당 작업을 시작한다. ",
    "각 Task의 첫 Read, Grep, Glob, Bash 등 작업 도구를 호출하기 전에 그 Task를 `in_progress`로 바꾸고, 그 작업이 끝난 직후 `completed`로 바꾼다. ",
    "종료 직전에 여러 Task를 한꺼번에 `completed`로 바꾸지 않는다.\n",
    "답변 형식 규칙:\n",
    "- 서론, 인사, 맺음말 요약을 쓰지 않고 결론부터 쓴다.\n",
    "- 이모티콘과 이모지를 쓰지 않는다. 답변, 진행 안내, Task 제목, 커밋 메시지 어디에도 넣지 않는다.\n",
    "- 산문 문단 대신 불릿과 코드 블록을 쓴다.\n",
    "- 코드 변경 보고에서도 파일 경로와 핵심 코드는 사용자 판단에 꼭 필요한 경우에만 최소한으로 보여주고, 요청받지 않은 해설을 덧붙이지 않는다.\n",
    "- 사용자에게 선택이나 승인을 요청할 때는 본문에 선택지를 나열하지 말고 반드시 AskUserQuestion 도구로 묻는다.\n",
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
    "- 최종 답변에는 직접적인 결론, 이를 뒷받침하는 핵심 근거만 담고, 확인 범위나 한계는 결론이 달라질 때만 덧붙인다. 내부 절차는 결과 판단에 필요할 때만 언급한다.\n",
    "- 조사나 수정 결과는 독립된 수정 하나당 불릿 하나와 짧은 문장 하나만 쓰고, 서로 다른 수정, 원인, 영향을 같은 불릿이나 문장에 묶지 않는다. 수정이 셋을 넘으면 중요한 셋만 쓰고 나머지는 개수만 밝힌다. 원인은 사용자가 물었거나 판단에 필요할 때만 쓰고, 확인하지 못했으면 미확인이라고 밝힌다. `수정했습니다`, `확인했습니다`만으로 결과를 끝내지 않는다.\n",
    "- 결론과 완료 보고는 바꾼 대상과 결과를 구체적으로 지목해 쓴다. `일부 수정했습니다`, `관련 부분을 개선했습니다`처럼 대상이 드러나지 않는 문장으로 얼버무리지 않는다.\n",
    "- 재개 기록, 사용자 질문, 권한 응답처럼 외부 상태를 기다리는 경우에는 실제 응답이나 오류를 받기 전 취소·거절·완료·원인을 단정하지 않는다. 질문 도구가 전달되지 않거나 응답을 받지 못했다는 오류가 오면 필요한 질문을 일반 text로 다시 보여 주고, 답이 필요한 작업은 사용자가 답하기 전 파일을 바꾸지 않는다.\n",
    "진행 보고 규칙:\n",
    "- 무엇을 알아냈는지 담기지 않은 진행 문장은 쓰지 않는다. ",
    "`다음 부분을 이어서 확인하겠습니다.`, `이어서 진행하겠습니다.`, `계속 확인하겠습니다.`처럼 ",
    "다음에 무엇을 왜 보는지 없는 문장은 같은 응답에서 한 번도 쓰지 않는다.\n",
    "- Skill 적용, 지침 확인, 내부 도구 호출 같은 내부 절차는 알리지 않는다.\n",
);

const CLAUDE_TURN_REMINDER: &str = "최종 답변은 불릿 2~3개, 전체 200자 내외로 쓰고 불릿 하나에 두 문장을 넘기지 않는다. 넘치면 덜 중요한 불릿을 통째로 지운다. 필요한 경우가 아니면 영어로 응답하지 않으며, 도구 호출 앞뒤 text도 첫 글자가 한글이어야 하고 영어 문장으로 시작하거나 영어 판정 뒤 한국어를 잇지 않는다. 클래스명·메서드명·변수명·파일 경로·코드 조각은 사용자 판단에 꼭 필요할 때만 최소로 쓴다.";

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
        model: if claude_model {
            state.selected_model_name().to_owned()
        } else {
            Default::default()
        },
        effort: if claude_model {
            state.selected_effort().to_owned()
        } else {
            Default::default()
        },
        permission_mode: state.claude_permission_mode_setting().wire().to_owned(),
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

/// Codex and Claude both read the full rules once — Codex as the thread's
/// developer instructions, Claude as the system prompt the bridge appends to its
/// preset. Claude also gets one short per-turn reminder for the response limits
/// it tends to miss. The full rules stay here for the one runtime with no
/// standing instructions of its own.
fn turn_additional_context(vibe: VibeMode, agent: Option<AgentTurnContext>) -> Value {
    let mut context = json!({
        "devez-vibe-rules": {
            "value": DEVEZ_INSTRUCTIONS,
            "kind": "application"
        },
        "claude-devez-vibe-rules": {
            "value": CLAUDE_DEVEZ_INSTRUCTIONS,
            "kind": "application"
        },
        "claude-devez-vibe-reminder": {
            "value": CLAUDE_TURN_REMINDER,
            "kind": "application"
        },
        "devez-vibe-mode": {
            "value": vibe.turn_notice(),
            "kind": "application"
        }
    });
    // Standard adds no key at all once its reset has landed, so a session that
    // never leaves Standard sends exactly what it sent before roles existed.
    if let Some(agent) = agent {
        context["devez-vibe-agent"] = json!({
            "value": agent.render(),
            "kind": "application"
        });
    }
    context
}

/// Every value Codex accepts for `sessionStartSource`. It rejects the whole
/// `thread/start` request on anything else, so a new source cannot be invented
/// to describe where the session came from.
const THREAD_START_SOURCES: [&str; 2] = ["startup", "clear"];

fn new_thread_params(
    cwd: &str,
    model: Option<&str>,
    service_tier: Option<&str>,
    session_start_source: &str,
    model_verbosity: &str,
    claude_permission_mode: &str,
    effort: &str,
) -> Value {
    debug_assert!(
        THREAD_START_SOURCES.contains(&session_start_source),
        "Codex rejects thread/start with an unknown session start source"
    );
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
    // Claude reads the shared top-level value, while Codex reads its thread
    // default from config. Keep both so the first response cannot replace the
    // effort selected before the provider-specific session exists.
    if !effort.is_empty() {
        params["effort"] = json!(effort);
        params["config"]["model_reasoning_effort"] = json!(effort);
    }
    params
}

async fn list_skills(
    server: &mut BackendServer,
    cwd: &str,
    force_reload: bool,
    provider: SkillProvider,
) -> Result<Value> {
    if provider == SkillProvider::Codex {
        server.start_codex().await?;
    }
    server
        .integration_request(
            provider.model_hint(),
            "skills/list",
            json!({
                "cwd": cwd,
                "cwds": [cwd],
                "forceReload": force_reload
            }),
        )
        .await
}

async fn open_skills(
    server: &mut BackendServer,
    state: &mut AppState,
    provider: SkillProvider,
    notice: Option<String>,
) {
    match list_skills(server, &state.cwd, true, provider).await {
        Ok(response) => state.open_skills_picker(provider, &response, notice),
        Err(error) => {
            let response = json!({
                "data": [{
                    "cwd": state.cwd.clone(),
                    "skills": [],
                    "errors": [{ "message": error.to_string() }]
                }]
            });
            state.open_skills_picker(provider, &response, notice);
        }
    }
}

async fn integration_request(
    server: &BackendServer,
    state: &AppState,
    method: &str,
    mut params: Value,
) -> Result<Value> {
    if let Some(object) = params.as_object_mut() {
        object
            .entry("cwd".to_owned())
            .or_insert_with(|| Value::String(state.cwd.clone()));
    }
    server
        .integration_request(state.selected_model_name(), method, params)
        .await
}

async fn list_plugins(server: &BackendServer, state: &AppState) -> Result<Value> {
    integration_request(
        server,
        state,
        "plugin/list",
        json!({
            "cwds": [state.cwd]
        }),
    )
    .await
}

async fn list_mcp_servers(server: &BackendServer, state: &AppState) -> Result<Value> {
    let thread_id = integration_mcp_thread_id(server, state)
        .context("현재 provider의 MCP 세션이 아직 시작되지 않았습니다.")?;
    server
        .integration_request(
            state.selected_model_name(),
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
            state.activate_first_opencode_model();
        }
        Err(error) => {
            state.provider_connected(provider_name);
            state.push_notice(BlockKind::Warning, "모델 새로고침 실패", error.to_string());
        }
    }
}

async fn write_plugin_enabled(
    server: &BackendServer,
    state: &AppState,
    plugin_id: &str,
    scope: Option<&str>,
    enabled: bool,
) -> Result<Value> {
    let (method, params) =
        plugin_write_request(state.selected_model_name(), plugin_id, scope, enabled);
    integration_request(server, state, method, params).await
}

fn plugin_write_request(
    model: &str,
    plugin_id: &str,
    scope: Option<&str>,
    enabled: bool,
) -> (&'static str, Value) {
    if claude::is_claude_model(model) {
        return (
            "plugin/set-enabled",
            json!({
                "pluginId": plugin_id,
                "scope": scope.unwrap_or("user"),
                "enabled": enabled
            }),
        );
    }
    (
        "config/value/write",
        json!({
            "keyPath": format!("plugins.{plugin_id}"),
            "value": { "enabled": enabled },
            "mergeStrategy": "upsert"
        }),
    )
}

fn mcp_toggle_request(
    provider: SkillProvider,
    thread_id: &str,
    name: &str,
    enabled: bool,
) -> (&'static str, Value) {
    match provider {
        SkillProvider::Claude => (
            "mcp/toggle",
            json!({
                "sessionId": thread_id,
                "name": name,
                "enabled": enabled
            }),
        ),
        SkillProvider::Codex => (
            "config/value/write",
            json!({
                "keyPath": format!("mcp_servers.{name}.enabled"),
                "value": enabled,
                "mergeStrategy": "upsert"
            }),
        ),
    }
}

async fn reopen_marketplaces(server: &BackendServer, state: &mut AppState, notice: String) {
    match list_plugins(server, state).await {
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
    if let Some(message) = response
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
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
    model: String,
    skills: std::result::Result<Value, String>,
    plugins: std::result::Result<Value, String>,
    apps: std::result::Result<Value, String>,
    mcp: std::result::Result<Value, String>,
}

async fn fetch_integrations(
    client: Option<IntegrationClient>,
    model: String,
    cwd: String,
    app_thread_id: Option<String>,
    mcp_thread_id: Option<String>,
    force_reload: bool,
) -> IntegrationCatalog {
    let Some(client) = client else {
        return IntegrationCatalog {
            model,
            skills: Ok(json!({ "data": [] })),
            plugins: Ok(json!({ "data": [] })),
            apps: Ok(json!({ "data": [] })),
            mcp: Ok(json!({ "data": [] })),
        };
    };
    let skills_client = client.clone();
    let plugins_client = client.clone();
    let mcp_client = client.clone();
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
    let mcp = async {
        let Some(thread_id) = mcp_thread_id else {
            return Ok(json!({
                "data": [],
                "unavailableReason": "provider 세션이 아직 시작되지 않았습니다."
            }));
        };
        mcp_client
            .request(
                "mcpServerStatus/list",
                json!({
                    "threadId": thread_id,
                    "detail": "toolsAndAuthOnly",
                    "limit": 100
                }),
            )
            .await
    };
    let (skills, plugins, apps, mcp) = tokio::join!(
        skills_client.request(
            "skills/list",
            json!({
                "cwd": cwd.clone(),
                "cwds": [cwd.clone()],
                "forceReload": force_reload
            }),
        ),
        plugins_client.request(
            "plugin/installed",
            json!({
                "cwd": cwd.clone(),
                "cwds": [cwd]
            }),
        ),
        apps,
        mcp,
    );
    IntegrationCatalog {
        model,
        skills: skills.map_err(|error| error.to_string()),
        plugins: plugins.map_err(|error| error.to_string()),
        apps: apps.map_err(|error| error.to_string()),
        mcp: mcp.map_err(|error| error.to_string()),
    }
}

fn apply_integrations(state: &mut AppState, catalog: IntegrationCatalog) -> Result<()> {
    let model = catalog.model;
    let is_current_provider = state.is_selected_provider_model(&model);
    let mut errors = Vec::new();
    match catalog.skills {
        Ok(response) if is_current_provider => state.update_skills(&response),
        Ok(_) => {}
        Err(error) => errors.push(format!("Skill 조회 실패: {error}")),
    }
    match catalog.plugins {
        Ok(response) => state.update_plugins_for_model(&response, &model),
        Err(error) => {
            state.note_plugin_query_error_for_model(&error, &model);
            errors.push(format!("플러그인 조회 실패: {error}"));
        }
    }
    match catalog.apps {
        Ok(response) if is_current_provider => state.update_apps(&response),
        Ok(_) => {}
        Err(error) => errors.push(format!("App 조회 실패: {error}")),
    }
    match catalog.mcp {
        Ok(response) => state.update_mcp_servers_for_model(&response, &model),
        Err(error) => {
            state.note_mcp_query_error_for_model(&error, &model);
            errors.push(format!("MCP 조회 실패: {error}"));
        }
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
        state::codex_home().and_then(|home| rollout::load_cost_ledger(&home, &lookup_thread_id))
    })
}

fn start_background_cost_restore(
    thread_id: String,
    restore: impl FnOnce() -> Option<pricing::CostLedger> + Send + 'static,
) -> mpsc::Receiver<CostRestore> {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        let ledger = tokio::task::spawn_blocking(restore).await.ok().flatten();
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
    let model = state.selected_model_name().to_owned();
    let app_thread_id = integration_app_thread_id(server, state);
    let mcp_thread_id = integration_mcp_thread_id(server, state);
    start_background_catalogue(fetch_integrations(
        server.integration_client(&model),
        model,
        state.cwd.clone(),
        app_thread_id,
        mcp_thread_id,
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

type SkillsResult = (String, std::result::Result<Value, String>);

/// Fetches only the skill list for the current provider in the background, so
/// `$` completion has skills without opening `/skills` first.
async fn start_skills_refresh(
    server: &mut BackendServer,
    state: &AppState,
) -> mpsc::Receiver<SkillsResult> {
    let model = state.selected_model_name().to_owned();
    let startup_error =
        if !claude::is_claude_model(&model) && !open_code::is_open_code_model(&model) {
            server
                .start_codex()
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
    let client = server.integration_client(&model);
    let open_code_api = open_code::is_open_code_model(&model)
        .then(|| server.open_code_provider_api())
        .flatten();
    let cwd = state.cwd.clone();
    let (sender, receiver) = mpsc::channel(1);
    let result_model = model.clone();
    tokio::spawn(async move {
        let result = if let Some(error) = startup_error {
            Err(error)
        } else if let Some(api) = open_code_api {
            api.skills().await.map_err(|error| error.to_string())
        } else if let Some(client) = client {
            client
                .request(
                    "skills/list",
                    json!({ "cwd": cwd.clone(), "cwds": [cwd], "forceReload": true }),
                )
                .await
                .map_err(|error| error.to_string())
        } else {
            Err("현재 provider의 Skill 조회 연결이 없습니다.".to_owned())
        };
        let _ = sender.send((result_model, result)).await;
    });
    receiver
}

async fn recv_skills(receiver: &mut Option<mpsc::Receiver<SkillsResult>>) -> Option<SkillsResult> {
    let Some(channel) = receiver.as_mut() else {
        return std::future::pending().await;
    };
    let result = channel.recv().await;
    if result.is_none() {
        *receiver = None;
    }
    result
}

async fn refresh_integrations(
    server: &BackendServer,
    state: &mut AppState,
    force_reload: bool,
) -> Result<()> {
    let model = state.selected_model_name().to_owned();
    let app_thread_id = integration_app_thread_id(server, state);
    let mcp_thread_id = integration_mcp_thread_id(server, state);
    let catalog = fetch_integrations(
        server.integration_client(&model),
        model,
        state.cwd.clone(),
        app_thread_id,
        mcp_thread_id,
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
    scope: String,
    source: Option<String>,
}

async fn write_skill_enabled(
    server: &BackendServer,
    cwd: &str,
    provider: SkillProvider,
    path: &str,
    source: Option<&str>,
    scope: &str,
    enabled: bool,
) -> Result<Value> {
    let (method, params) = skill_write_request(cwd, provider, path, source, scope, enabled)?;
    server
        .integration_request(provider.model_hint(), method, params)
        .await
}

fn skill_write_request(
    cwd: &str,
    provider: SkillProvider,
    path: &str,
    source: Option<&str>,
    scope: &str,
    enabled: bool,
) -> Result<(&'static str, Value)> {
    let (method, params) = match provider {
        SkillProvider::Claude => {
            let plugin_id = source.context(
                "개인·프로젝트 스킬은 파일이 있으면 항상 켜져 있어 여기서 끌 수 없습니다.",
            )?;
            (
                "plugin/set-enabled",
                json!({
                    "cwd": cwd,
                    "pluginId": plugin_id,
                    "scope": scope,
                    "enabled": enabled
                }),
            )
        }
        SkillProvider::Codex => (
            "skills/config/write",
            json!({
                "name": null,
                "path": path,
                "enabled": enabled
            }),
        ),
    };
    Ok((method, params))
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
            scope: skill
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_owned(),
            source: skill
                .get("pluginId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    child_process::isolate_launcher(&mut command);
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
                "body": block.body,
                "responseDurationMs": block.response_duration_ms
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
    renderer: &mut Renderer,
    text: String,
    provider_handoff: Option<Value>,
) -> Result<()> {
    devezcode::note_prompt(&text);
    let model = state.selected_model_name().to_owned();
    let effort = state.selected_effort().to_owned();
    state.note_pending_turn_model(&model);
    state.note_pending_turn_effort(&effort);
    let input = state.turn_input(text);
    let agent_context = state.next_agent_context();
    let mut params = json!({
        "threadId": state.thread_id,
        "input": input,
        "model": model,
        "serviceTier": state.service_tier(),
        "permissions": state.permission_profile(),
        "additionalContext": turn_additional_context(state.vibe_mode(), agent_context)
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
    match await_with_activity(state, renderer, server.request("turn/start", params)).await? {
        // The response reserves an id, but the app-server makes it
        // interruptible only after the subsequent `turn/started` notification.
        // The role is recorded here rather than at build time so a failed send
        // leaves a Standard reset still owed.
        Ok(_) => state.note_agent_dispatch_succeeded(agent_context),
        Err(error) => state.set_request_failed(error.to_string()),
    }
    Ok(())
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
    if clipboard_text()
        .is_none_or(|text| paste::paste_payload_chars(&text) != paste::paste_payload_chars(&block))
    {
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

/// Reads ordinary clipboard text directly instead of waiting for Windows
/// Terminal to re-create it as key records. That input path can lose emoji
/// sequences made of surrogate pairs, variation selectors, or ZWJ joins.
/// The expected payload is then swallowed when the terminal also forwards it.
fn paste_clipboard_text_shortcut(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: &KeyEvent,
    now: Instant,
) -> bool {
    if !is_paste_shortcut(key) || state.has_pending_interaction() {
        return false;
    }
    let Some(text) = clipboard_text().filter(|text| !text.is_empty()) else {
        return false;
    };
    apply_clipboard_text_paste(state, buffer, &text, now)
}

fn apply_clipboard_text_paste(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    text: &str,
    now: Instant,
) -> bool {
    if text.is_empty() {
        return false;
    }
    apply_direct_paste(state, text);
    buffer.discard_expected(text, now);
    true
}

/// Some Windows terminals do not expose the Ctrl+V key at all. For emoji that
/// depend on surrogate pairs or joiners, use the clipboard as soon as the first
/// synthesized character proves that it is the payload, then consume that key
/// and the rest of the duplicate stream.
fn apply_fragile_clipboard_paste(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: &KeyEvent,
    text: Option<&str>,
    now: Instant,
) -> bool {
    if state.has_pending_interaction() || buffer.is_buffering() {
        return false;
    }
    let Some(text) = text.filter(|text| fragile_clipboard_text(text)) else {
        return false;
    };
    if paste::paste_payload_chars(text).first().copied() != paste::payload_char(key) {
        return false;
    }
    apply_clipboard_text_paste(state, buffer, text, now);
    let _ = buffer.observe_expected(*key, now, None);
    true
}

fn fragile_clipboard_text(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}') || u32::from(ch) > 0xffff)
}

fn apply_fragile_clipboard_paste_from_key(
    state: &mut AppState,
    buffer: &mut ComposerPasteBuffer,
    key: &KeyEvent,
    now: Instant,
) -> bool {
    if paste::payload_char(key).is_none_or(|ch| u32::from(ch) <= 0xffff) {
        return false;
    }
    let text = clipboard_text();
    apply_fragile_clipboard_paste(state, buffer, key, text.as_deref(), now)
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
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// The composer characters an edit typed over the selection should take. Ctrl+A
/// means the whole prompt, and answering from the composer rather than from the
/// painted cells keeps that true while a paint is still catching up — an IME
/// commits its syllable well after the key that started it.
fn composer_replace_range(
    renderer: &Renderer,
    state: &AppState,
) -> Option<std::ops::Range<usize>> {
    if renderer.composer_select_all_active() {
        let end = state.editor.chars().len();
        return (end > 0).then_some(0..end);
    }
    renderer.composer_selection_range()
}

/// Ctrl+X over selected composer text cuts it. With a Korean IME on, the chord
/// can arrive as its 두벌식 jamo, which nothing else claims.
fn is_cut_shortcut(key: &KeyEvent) -> bool {
    matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) && matches!(key.code, KeyCode::Char('x' | 'X' | 'ㅌ'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// A plain character typed over selected composer text replaces it, as it does in
/// any other editor. Chords are excluded: they carry their own meanings, and Enter
/// submits rather than edits, so neither disturbs the selection here.
fn is_selection_replace_key(key: &KeyEvent) -> bool {
    matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) && matches!(key.code, KeyCode::Char(_))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
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

fn apply_composer_inputs_with_scroll(
    state: &mut AppState,
    renderer: &mut Renderer,
    inputs: Vec<ComposerInput>,
) -> Action {
    let mut action = Action::None;
    for input in inputs {
        action = match input {
            ComposerInput::Key(key) => composer_vertical_move(state, renderer, &key)
                .unwrap_or_else(|| match scroll_request(renderer, &key) {
                    Some(delta) => Action::Tick(renderer.scroll(delta)),
                    None => state.handle_key(key),
                }),
            ComposerInput::Text(text) => {
                apply_composer_text(state, text);
                Action::None
            }
        };
    }
    action
}

fn integration_mcp_thread_id(server: &BackendServer, state: &AppState) -> Option<String> {
    if claude::is_claude_model(state.selected_model_name()) {
        Some(state.thread_id.clone())
    } else {
        server.codex_thread_id(&state.thread_id)
    }
}

fn composer_vertical_move(
    state: &mut AppState,
    renderer: &Renderer,
    key: &KeyEvent,
) -> Option<Action> {
    if state.has_pending_interaction()
        || key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER,
        )
    {
        return None;
    }
    let delta = match key.code {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => return None,
    };
    let target =
        renderer.composer_vertical_cursor_position(state.editor.display_cursor(), delta)?;
    state
        .editor
        .move_to_display_index_preserving_history(target);
    Some(Action::None)
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
            buffer.observe_targeted(key, now, BufferedTextTarget::PendingUserInput(target)),
        )
    } else if state.has_pending_interaction() {
        state.handle_key(key)
    } else {
        arm_verified_collapsed_paste(state, buffer, &key, now);
        if apply_fragile_clipboard_paste_from_key(state, buffer, &key, now) {
            return Action::None;
        }
        let expected_paste = state.editor.collapsed_paste_text();
        apply_composer_inputs_with_scroll(
            state,
            renderer,
            buffer.observe_expected(key, now, expected_paste.as_deref()),
        )
    }
}

/// Empties the batch immediately, for the one case that cannot wait for it to go
/// idle: the character typed over a selection, which has to appear in the same
/// frame the selected text disappears from.
fn flush_composer_paste_now(state: &mut AppState, buffer: &mut ComposerPasteBuffer) -> bool {
    if let Some(text) = buffer.flush_now() {
        apply_composer_text(state, text);
        true
    } else {
        false
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
    let discarded_prompt_ids = state.take_discarded_prompt_ids();
    renderer.remove_history_blocks(&discarded_prompt_ids)?;
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
    if result.is_ok() {
        state.note_response_frame_rendered(&committed);
    }
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
/// Asks the runtime whether the turn the activity row is waiting on is still
/// running. Every uncertain answer — an unsupported runtime, an error, a reply
/// that never comes — reads as "running", so the wait is only ever ended by the
/// runtime saying so outright.
async fn turn_is_running(server: &BackendServer, thread_id: &str, turn_id: &str) -> bool {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    let params = json!({ "threadId": thread_id, "turnId": turn_id });
    let Ok(Ok(status)) = timeout(PROBE_TIMEOUT, server.request("turn/status", params)).await else {
        return true;
    };
    if status.get("known").and_then(Value::as_bool) != Some(true) {
        return true;
    }
    status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// Codex does not emit a periodic child heartbeat. Re-read every quiet child in
/// parallel so a long command remains visible while a missed terminal event is
/// still bounded. Unknown/error replies are deliberately left to the state's
/// retry counter rather than guessed terminal here.
async fn codex_subagent_statuses(
    server: &BackendServer,
    ids: Vec<String>,
) -> Vec<(String, Option<bool>)> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    let Some(client) = server.client() else {
        return ids.into_iter().map(|id| (id, None)).collect();
    };
    futures_util::future::join_all(ids.into_iter().map(|id| {
        let client = client.clone();
        async move {
            let params = json!({ "threadId": id.clone() });
            let running = match timeout(PROBE_TIMEOUT, client.request("thread/read", params)).await
            {
                Ok(Ok(response)) => codex_thread_running(&response),
                _ => None,
            };
            (id, running)
        }
    }))
    .await
}

fn codex_thread_running(response: &Value) -> Option<bool> {
    match response
        .pointer("/thread/status/type")
        .and_then(Value::as_str)
    {
        Some("active") => Some(true),
        Some("idle" | "notLoaded" | "systemError") => Some(false),
        _ => None,
    }
}

async fn refresh_account(server: &BackendServer, state: &mut AppState) {
    let model = state.selected_model_name().to_owned();
    // OpenCode 계정은 opencode CLI가 관리한다. Codex의 account/read를 빌려
    // ChatGPT 이메일을 보여 주지 않는다.
    if open_code::is_open_code_model(&model) {
        state.set_account("OpenCode CLI".to_owned());
        state.set_account_plan(AccountPlan::default());
        return;
    }
    if claude::is_claude_model(&model) {
        if let Ok(account) = server
            .request("claude/account/read", json!({ "cwd": state.cwd }))
            .await
            && let Some(label) = claude_account_label(&account)
        {
            state.set_account(label);
        }
        return;
    }
    if let Ok(label) = ensure_account(server).await {
        state.set_account(label);
    }
    state.set_account_plan(read_runtime_account_plan(server, &model).await);
}

fn claude_account_label(account: &Value) -> Option<String> {
    if account.get("loggedIn").and_then(Value::as_bool) == Some(false) {
        return Some("signed out".to_owned());
    }
    account
        .get("email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            account
                .get("authMethod")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| format!("Claude {value}"))
        })
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
    if claude::is_claude_model(model) || open_code::is_open_code_model(model) {
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
        request_resume_thread(server, params).await
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

const ACTIVE_WRITER_RESUME_ERROR: &str = "already has an active writer";
const ACTIVE_WRITER_RESUME_NOTICE: &str =
    "이 대화는 다른 Codex 창에서 사용 중입니다. 기존 대화를 닫은 뒤 다시 시도하세요.";

async fn request_resume_thread(server: &BackendServer, params: Value) -> Result<Value> {
    server
        .request("thread/resume", params)
        .await
        .map_err(|error| anyhow::anyhow!(resume_error_message(&error.to_string())))
}

fn resume_error_message(error: &str) -> String {
    if error.contains(ACTIVE_WRITER_RESUME_ERROR) {
        ACTIVE_WRITER_RESUME_NOTICE.to_owned()
    } else {
        error.to_owned()
    }
}

async fn list_sessions(
    server: &BackendServer,
    cwd: Option<&Path>,
    search: Option<&str>,
    limit: u64,
    provider_model: Option<&str>,
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
    if let Some(model) = provider_model {
        params["provider"] = json!(session_provider(model));
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
    provider_model: Option<&str>,
) -> Result<String> {
    if looks_like_thread_id(target) {
        return Ok(target.to_owned());
    }

    let sessions = list_sessions(server, cwd, Some(target), 100, provider_model).await?;
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

fn session_provider(model: &str) -> &'static str {
    if claude::is_claude_model(model) {
        "claude"
    } else if open_code::is_open_code_model(model) {
        "opencode"
    } else {
        "codex"
    }
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
    fn claude_account_status_prefers_email_and_reports_signed_out() {
        assert_eq!(
            claude_account_label(&json!({
                "loggedIn": true,
                "authMethod": "claude.ai",
                "email": "claude@example.com"
            }))
            .as_deref(),
            Some("claude@example.com")
        );
        assert_eq!(
            claude_account_label(&json!({ "loggedIn": false })).as_deref(),
            Some("signed out")
        );
    }

    #[test]
    fn codex_child_thread_status_distinguishes_running_terminal_and_unknown() {
        assert_eq!(
            codex_thread_running(&json!({ "thread": { "status": { "type": "active" } } })),
            Some(true)
        );
        for terminal in ["idle", "notLoaded", "systemError"] {
            assert_eq!(
                codex_thread_running(&json!({ "thread": { "status": { "type": terminal } } })),
                Some(false)
            );
        }
        assert_eq!(codex_thread_running(&json!({ "thread": {} })), None);
    }

    /// Drives a key through the same path the event loop uses. The renderer is
    /// the real one so scroll and vertical-move keys resolve as they do in the
    /// app instead of against a second, drifting copy of the branch.
    fn observe_key(
        state: &mut AppState,
        buffer: &mut ComposerPasteBuffer,
        key: KeyEvent,
        now: Instant,
    ) -> Action {
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        observe_composer_key_with_scroll(state, &mut renderer, buffer, key, now)
    }

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
        assert_eq!(
            plain_windows_path(PathBuf::from(&long)),
            PathBuf::from(long)
        );
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
    fn clipboard_text_paste_keeps_joined_emoji_and_discards_duplicate_events() {
        let mut state = starting_state();
        let mut buffer = ComposerPasteBuffer::new();
        let text = "확인 👨‍👩‍👧‍👦 ✈️";

        assert!(apply_clipboard_text_paste(
            &mut state,
            &mut buffer,
            text,
            Instant::now(),
        ));
        assert_eq!(state.editor.text(), text);
        assert!(buffer.take_discarded_paste(text));
        assert!(!buffer.take_discarded_paste(text));
    }

    #[test]
    fn empty_clipboard_text_is_not_applied() {
        let mut state = starting_state();
        let mut buffer = ComposerPasteBuffer::new();

        assert!(!apply_clipboard_text_paste(
            &mut state,
            &mut buffer,
            "",
            Instant::now(),
        ));
        assert!(state.editor.text().is_empty());
    }

    #[test]
    fn first_surrogate_pair_key_applies_the_full_clipboard_emoji() {
        let mut state = starting_state();
        let mut buffer = ComposerPasteBuffer::new();
        let text = "👨‍👩‍👧‍👦";
        let key = press(KeyCode::Char('👨'), KeyModifiers::NONE);

        assert!(apply_fragile_clipboard_paste(
            &mut state,
            &mut buffer,
            &key,
            Some(text),
            Instant::now(),
        ));
        assert_eq!(state.editor.text(), text);
        assert!(buffer.take_discarded_paste(text));
    }

    #[test]
    fn ordinary_clipboard_text_does_not_take_the_emoji_fallback() {
        assert!(!fragile_clipboard_text("ordinary text"));
        assert!(fragile_clipboard_text("✈️"));
    }

    #[test]
    fn clicking_the_response_display_badge_opens_its_picker() {
        let mut state = starting_state();
        let before = state.response_display_mode();

        let action = pick_action(&mut state, Pick::ResponseDisplayMode);

        assert!(matches!(action, Action::None));
        assert_eq!(state.response_display_mode(), before);
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Response".to_owned())
        );
    }

    #[test]
    fn clicking_the_fast_badge_opens_the_fast_picker() {
        let mut state = state_with_a_model();

        let action = pick_action(&mut state, Pick::FastMode);

        assert!(matches!(action, Action::None));
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Fast".to_owned())
        );
    }

    #[test]
    fn clicking_the_vibe_badge_opens_its_picker() {
        let mut state = starting_state();
        let before = state.vibe_mode();

        let action = pick_action(&mut state, Pick::VibeMode);

        assert!(matches!(action, Action::None));
        assert_eq!(state.vibe_mode(), before);
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Vibe Mode".to_owned())
        );
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
    fn clicking_a_recent_prompt_requests_its_transcript_jump() {
        let mut state = starting_state();

        assert!(matches!(
            pick_action(&mut state, Pick::Prompt(42)),
            Action::ScrollToPrompt(42)
        ));
    }

    #[test]
    fn clicking_the_prompt_section_title_toggles_its_rows() {
        let mut state = starting_state();
        assert!(state.view().side_panel_prompts_expanded);

        assert!(matches!(
            pick_action(&mut state, Pick::PromptSection),
            Action::Tick(true)
        ));
        assert!(!state.view().side_panel_prompts_expanded);

        pick_action(&mut state, Pick::PromptSection);
        assert!(state.view().side_panel_prompts_expanded);
    }

    #[test]
    fn clicking_integration_section_titles_toggles_their_rows() {
        let mut state = starting_state();
        let codex = || "Codex".to_owned();

        assert!(
            state
                .view()
                .side_panel_integrations
                .iter()
                .find(|view| view.provider == "Codex")
                .is_some_and(|view| view.mcp_expanded && view.plugins_expanded)
        );

        assert!(matches!(
            pick_action(&mut state, Pick::McpSection(codex())),
            Action::Tick(true)
        ));
        assert!(matches!(
            pick_action(&mut state, Pick::PluginSection(codex())),
            Action::Tick(true)
        ));
        assert!(
            state
                .view()
                .side_panel_integrations
                .iter()
                .find(|view| view.provider == "Codex")
                .is_some_and(|view| !view.mcp_expanded && !view.plugins_expanded)
        );
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
                supports_auto_mode: false,
            }],
            "gpt-5.6-sol",
            None,
        )
    }

    #[test]
    fn split_focus_routes_input_and_thread_events_to_the_matching_pane() {
        let mut main = state_with_a_model();
        main.thread_id = "main-thread".to_owned();
        let mut btw = Some(main.forked_side_state(
            "btw-thread".to_owned(),
            main.cwd.clone(),
            main.selected_model_name(),
            Some(main.selected_effort()),
        ));

        focused_state_mut(&mut main, &mut btw, SplitFocus::Main)
            .editor
            .set_text("main");
        focused_state_mut(&mut main, &mut btw, SplitFocus::Btw)
            .editor
            .set_text("btw");

        assert_eq!(main.editor.text(), "main");
        assert_eq!(btw.as_ref().expect("BTW pane").editor.text(), "btw");
        assert_eq!(
            event_thread_id(&json!({ "threadId": "btw-thread" })),
            Some("btw-thread")
        );
        assert_eq!(
            event_thread_id(&json!({ "turn": { "threadId": "main-thread" } })),
            Some("main-thread")
        );
    }

    #[test]
    fn btw_temporarily_collapses_and_restores_the_main_plan() {
        let mut main = state_with_a_model();
        main.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "inProgress" }] }),
        );
        assert_eq!(main.plan_summary_expanded(), Some(true));

        let mut saved = collapse_main_plan_for_btw(&mut main);
        assert_eq!(saved, Some(true));
        assert_eq!(main.plan_summary_expanded(), Some(false));

        restore_main_plan_after_btw(&mut main, &mut saved);
        assert_eq!(main.plan_summary_expanded(), Some(true));
        assert_eq!(saved, None);

        main.set_plan_summary_expanded(false);
        let mut saved = collapse_main_plan_for_btw(&mut main);
        restore_main_plan_after_btw(&mut main, &mut saved);
        assert_eq!(main.plan_summary_expanded(), Some(false));
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
            supports_auto_mode: false,
        }
    }

    #[test]
    fn unavailable_codex_falls_back_unless_an_available_provider_was_requested() {
        assert!(!should_fallback_to_claude(false, None));
        assert!(should_fallback_to_claude(false, Some("gpt-5.6-sol")));
        assert!(!should_fallback_to_claude(false, Some("claude:sonnet")));
        assert!(!should_fallback_to_claude(
            false,
            Some("opencode:provider/model")
        ));
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

    #[test]
    fn up_moves_through_wrapped_composer_rows_before_recalling_history() {
        let mut state = starting_state();
        state.editor.set_text("remembered prompt");
        state.editor.take_for_submit();
        state.editor.set_text("abcdefghijkl");
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        renderer.set_composer_navigation_layout_for_test(&state.editor, 18);

        apply_composer_inputs_with_scroll(
            &mut state,
            &mut renderer,
            vec![ComposerInput::Key(press(KeyCode::Up, KeyModifiers::NONE))],
        );
        assert_eq!(state.editor.text(), "abcdefghijkl");
        assert_eq!(state.editor.display_cursor(), 4);
        assert_eq!(state.editor.history_position(), None);

        apply_composer_inputs_with_scroll(
            &mut state,
            &mut renderer,
            vec![ComposerInput::Key(press(KeyCode::Up, KeyModifiers::NONE))],
        );
        assert_eq!(state.editor.text(), "remembered prompt");
        assert!(state.editor.history_position().is_some());
    }

    /// Alt+P steps the panel through its widths and wraps closed on the
    /// fourth press, without ever leaving a stray letter in the composer.
    #[test]
    fn alt_p_cycles_the_side_panel_through_its_widths_without_editing_the_composer() {
        let mut state = AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![model("gpt-5.6-sol", "high", true, &["low", "high"])],
            "gpt-5.6-sol",
            Some("high"),
        );
        let mut renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);
        while state.side_panel_stage() != state::SidePanelStage::Closed {
            state.cycle_side_panel();
        }
        // Goes through the paste burst buffer, which is what swallows a bare
        // printable key before the shortcut branches ever run.
        let mut paste = ComposerPasteBuffer::new();
        let now = Instant::now();
        let press_alt_p =
            |state: &mut AppState, renderer: &mut Renderer, paste: &mut ComposerPasteBuffer| {
                observe_composer_key_with_scroll(
                    state,
                    renderer,
                    paste,
                    press(KeyCode::Char('p'), KeyModifiers::ALT),
                    now,
                );
            };

        press_alt_p(&mut state, &mut renderer, &mut paste);
        assert_eq!(state.side_panel_stage(), state::SidePanelStage::Small);
        assert!(state.editor.text().is_empty());

        press_alt_p(&mut state, &mut renderer, &mut paste);
        assert_eq!(state.side_panel_stage(), state::SidePanelStage::Medium);

        press_alt_p(&mut state, &mut renderer, &mut paste);
        assert_eq!(state.side_panel_stage(), state::SidePanelStage::Large);

        press_alt_p(&mut state, &mut renderer, &mut paste);
        assert_eq!(state.side_panel_stage(), state::SidePanelStage::Closed);
        assert!(!state.side_panel_open());
        assert!(state.editor.text().is_empty());
    }

    /// The slash command opens a size picker, then asks whether the selection
    /// belongs to this session or becomes the default.
    #[test]
    fn the_side_panel_slash_command_picks_a_size_and_scope() {
        let mut state = AppState::new(
            String::new(),
            ".".to_owned(),
            "tester".to_owned(),
            vec![model("gpt-5.6-sol", "high", true, &["low", "high"])],
            "gpt-5.6-sol",
            Some("high"),
        );
        while state.side_panel_stage() != state::SidePanelStage::Closed {
            state.cycle_side_panel();
        }

        state.run_slash_command("/side-panel");
        let picker = state.view().overlay.expect("side-panel picker");
        assert_eq!(picker.title, "Side panel");
        assert_eq!(
            picker.slider.expect("size choices").efforts,
            ["Off", "Small", "Medium", "Large"]
        );

        state.handle_key(press(KeyCode::Right, KeyModifiers::NONE));
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            state.view().overlay.map(|overlay| overlay.title),
            Some("Apply to".to_owned())
        );

        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.side_panel_stage(), state::SidePanelStage::Small);
        assert!(state.view().overlay.is_none());
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
    fn page_keys_scroll_a_fullscreen_transcript_by_one_viewport() {
        let renderer = Renderer::new(ThemeKind::Dark, RenderMode::Fullscreen);

        assert_eq!(
            scroll_request(&renderer, &press(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(renderer.page_rows())
        );
        assert_eq!(
            scroll_request(&renderer, &press(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(-renderer.page_rows())
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
    fn fresh_threads_include_the_model_and_effort_selected_before_first_prompt() {
        let params = new_thread_params(
            "C:\\repo",
            Some("gpt-5.6-terra"),
            None,
            "startup",
            "low",
            "default",
            "high",
        );

        assert_eq!(
            params
                .pointer("/developerInstructions")
                .and_then(Value::as_str),
            Some(DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params
                .pointer("/claudeDeveloperInstructions")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params.pointer("/model").and_then(Value::as_str),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            params
                .pointer("/config/model_reasoning_effort")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn fast_update_is_built_only_after_thread_start_binds_an_id() {
        assert!(fast_settings_update_params("", "priority").is_none());

        let params = fast_settings_update_params("thread-1", "priority")
            .expect("a bound thread can receive its Fast setting");
        assert_eq!(
            params.pointer("/threadId").and_then(Value::as_str),
            Some("thread-1")
        );
        assert_eq!(
            params.pointer("/serviceTier").and_then(Value::as_str),
            Some("priority")
        );
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

        assert_eq!(
            params.pointer("/threadId").and_then(Value::as_str),
            Some("thread-1")
        );
        assert_eq!(
            params
                .pointer("/developerInstructions")
                .and_then(Value::as_str),
            Some(DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params
                .pointer("/claudeDeveloperInstructions")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            params
                .pointer("/initialTurnsPage/itemsView")
                .and_then(Value::as_str),
            Some("full")
        );
    }

    #[test]
    fn an_active_writer_resume_error_explains_how_to_retry() {
        let error = "thread/resume: thread 019ff007 already has an active writer (-32600)";

        assert_eq!(resume_error_message(error), ACTIVE_WRITER_RESUME_NOTICE);
    }

    #[test]
    fn other_resume_errors_keep_the_runtime_message() {
        let error = "thread/resume: no rollout found for thread id 019ff007";

        assert_eq!(resume_error_message(error), error);
    }

    fn test_claude_settings() -> ClaudeSessionSettings {
        ClaudeSessionSettings {
            model: "claude:opus".to_owned(),
            effort: "xhigh".to_owned(),
            permission_mode: "bypassPermissions".to_owned(),
        }
    }

    /// The saved default rides along as a fallback only: forcing it into `model`
    /// would outrank what the resumed thread's own turns ran on.
    #[test]
    fn a_resumed_thread_carries_the_saved_defaults_as_fallbacks() {
        let params = resume_thread_params("claude:session-1", &test_claude_settings());

        assert_eq!(
            params
                .pointer("/claudeFallbackModel")
                .and_then(Value::as_str),
            Some("claude:opus")
        );
        assert_eq!(
            params
                .pointer("/claudeFallbackEffort")
                .and_then(Value::as_str),
            Some("xhigh")
        );
        assert_eq!(
            params
                .pointer("/claudePermissionMode")
                .and_then(Value::as_str),
            Some("bypassPermissions")
        );
        assert!(params.get("model").is_none());
        assert!(params.get("effort").is_none());
    }

    /// Nothing else in a turn carries which preset is active. The preset no
    /// longer changes how the answer is written, so what rides along is the
    /// language rule and the exception the length cap kept breaking.
    #[test]
    fn every_turn_names_the_active_preset() {
        let notice = |vibe| {
            turn_additional_context(vibe, None)
                .pointer("/devez-vibe-mode/value")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .expect("the turn names its preset")
        };

        assert!(notice(VibeMode::SuperVibe).contains("Super Vibe"));
        assert!(notice(VibeMode::Vibe).contains("현재 응답 모드: Vibe"));
        assert!(notice(VibeMode::Normal).contains("현재 응답 모드: Off"));
        // One length rule now holds in every mode, and it lives in the standing
        // rules. A per-preset cap here would only contradict it.
        for vibe in [VibeMode::Vibe, VibeMode::SuperVibe, VibeMode::Normal] {
            assert!(!notice(vibe).contains("불릿"));
            assert!(!notice(vibe).contains("세 줄"));
            assert!(!notice(vibe).contains("파일 경로"));
            assert!(!notice(vibe).contains("자세히"));
        }
        // The English tool-call label leaks through the system prompt, so every
        // preset repeats the language rule where the turn cannot miss it.
        for vibe in [VibeMode::Vibe, VibeMode::SuperVibe, VibeMode::Normal] {
            assert!(notice(vibe).contains("영어로 시작하는 진행 문장"));
            assert!(notice(vibe).contains("첫 글자가 한글 음절이어야 하고"));
            assert!(!notice(vibe).contains("Now"));
            assert!(
                notice(vibe).contains("선택이나 승인을 요청할 때는 이 분량 제한을 적용하지 않는다")
            );
            assert!(notice(vibe).contains("AskUserQuestion 도구를 쓸 수 있으면"));
        }
        // The cap the exception refers to, stated once for every mode.
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("불릿 두세 개, 전체 200자 내외"));
            assert!(rules.contains("불릿 하나에 두 문장을 넘기지 않는다"));
            assert!(!rules.contains("세 줄"));
        }
    }

    /// The role travels as its own application context key, and a Standard turn
    /// with nothing owed adds no key at all.
    #[test]
    fn the_turn_carries_the_selected_role_and_nothing_when_standard() {
        let planner = turn_additional_context(
            VibeMode::Vibe,
            Some(AgentTurnContext::Specialized(agent::AgentMode::Planner)),
        );
        let block = planner
            .pointer("/devez-vibe-agent/value")
            .and_then(Value::as_str)
            .expect("a specialized turn names its role");
        assert!(block.starts_with("<devez-vibe-agent mode=\"planner\""));
        assert_eq!(
            planner
                .pointer("/devez-vibe-agent/kind")
                .and_then(Value::as_str),
            Some("application")
        );

        let reset = turn_additional_context(VibeMode::Vibe, Some(AgentTurnContext::StandardReset));
        assert!(
            reset
                .pointer("/devez-vibe-agent/value")
                .and_then(Value::as_str)
                .is_some_and(|block| block.contains("mode=\"builder\""))
        );

        assert!(
            turn_additional_context(VibeMode::Vibe, None)
                .get("devez-vibe-agent")
                .is_none()
        );
    }

    #[test]
    fn every_turn_restates_the_rules() {
        let context = turn_additional_context(VibeMode::Vibe, None);

        assert_eq!(
            context
                .pointer("/devez-vibe-rules/value")
                .and_then(Value::as_str),
            Some(DEVEZ_INSTRUCTIONS)
        );
        assert_eq!(
            context
                .pointer("/devez-vibe-rules/kind")
                .and_then(Value::as_str),
            Some("application")
        );
        assert_eq!(
            context
                .pointer("/claude-devez-vibe-rules/value")
                .and_then(Value::as_str),
            Some(CLAUDE_DEVEZ_INSTRUCTIONS)
        );
        // Both providers hold the full rules already. Claude alone gets a short
        // reminder for the output limits it repeatedly misses.
        assert!(context.get("codex-devez-vibe-reminder").is_none());
        assert_eq!(
            context
                .pointer("/claude-devez-vibe-reminder/value")
                .and_then(Value::as_str),
            Some(CLAUDE_TURN_REMINDER)
        );
        assert!(CLAUDE_TURN_REMINDER.contains("불릿 2~3개, 전체 200자 내외"));
        assert!(CLAUDE_TURN_REMINDER.contains("불릿 하나에 두 문장을 넘기지 않는다"));
        assert!(CLAUDE_TURN_REMINDER.contains("필요한 경우가 아니면 영어로 응답하지 않으며"));
        assert!(CLAUDE_TURN_REMINDER.contains("도구 호출 앞뒤 text도 첫 글자가 한글이어야 하고"));
        assert!(CLAUDE_TURN_REMINDER.contains("사용자 판단에 꼭 필요할 때만 최소로 쓴다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("TaskCreate"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 응답 content block"));
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS
                .contains("요청 내용을 확인하고 필요한 작업을 진행하겠습니다")
        );
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("대상·근거·행동이 없는 포괄적 접수 문구"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("모든 일반 문장은 반드시 한국어로 작성한다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("영어 부사·접속사로 문장을 시작하는 형태"));
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS.contains("어떤 tool_use도 이 text보다 먼저 출력하지 않는다")
        );
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("두 번째 작업 도구를 호출하거나"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("TaskCreate를 대신하지 않는다"));
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS.contains("독립된 수정 하나당 불릿 하나와 짧은 문장 하나")
        );
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS
                .contains("서로 다른 수정, 원인, 영향을 같은 불릿이나 문장에 묶지 않는다")
        );
        assert!(DEVEZ_INSTRUCTIONS.contains("확인된 원인, 사용자에게 미치는 영향, 실제 조치"));
        assert!(DEVEZ_INSTRUCTIONS.contains("`update_plan`을 대신하지 않는다"));
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS.contains("모든 TaskCreate의 subject에는 반드시 제목 자체")
        );
        assert!(DEVEZ_INSTRUCTIONS.contains("`update_plan`의 각 step에는 반드시 제목 자체"));
        // The length cap is the same in every mode, so it is stated here once
        // rather than deferred to the preset notice.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("응답 모드와 관계없이"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("완료 문구를 붙이지 않는다"));
            assert!(rules.contains("`~한 내용을 완료했습니다.`처럼 명사절을 겹쳐 쓰거나"));
            assert!(!rules.contains("`~ 내용을 완료했습니다.` 형식으로"));
        }
        // The preset caps cut the choices out of the very answer that exists to
        // present them, so each provider gets the asking form it can actually use.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("반드시 AskUserQuestion 도구로 묻는다"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("선택지가 다섯 개 이상이라"));
        assert!(
            DEVEZ_INSTRUCTIONS
                .contains("선택이나 승인을 요청하는 답변에는 이 분량 제한을 적용하지 않는다")
        );
        assert!(DEVEZ_INSTRUCTIONS.contains("선택지를 줄이거나 문장을 도중에 끊지 않는다"));
        // Read as a per-call duty, the opening notice turned into the same
        // contentless line before every tool call.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 assistant message에만 적용한다"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("`다음 부분을 이어서 확인하겠습니다.`"));
        }
        // Spelling the banned opener out five times primed the very word it
        // banned, so the rule is stated positively and the token appears nowhere.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 글자가 한글 음절이어야 한다"));
        assert!(!CLAUDE_DEVEZ_INSTRUCTIONS.contains("Now"));
        // The ban only held when it moved above the format rules and named the
        // two shapes that actually leaked: an English label glued in front of a
        // Korean sentence, and an English verdict on a tool result. Saying it
        // three times in three sections did not help, so it is stated once.
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("최우선 한국어 전용 규칙"));
        assert!(
            CLAUDE_DEVEZ_INSTRUCTIONS.contains("영어 낱말로 문장을 시작한 뒤 한국어를 이어 붙이는")
        );
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("`Good, that closes correctly.`"));
        assert_eq!(
            CLAUDE_DEVEZ_INSTRUCTIONS
                .matches("응답 언어는 한국어로 유지한다")
                .count(),
            1
        );
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("첫 작업 도구 호출 전에"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("도구를 두 번 이상 호출할 작업"));
        assert!(DEVEZ_INSTRUCTIONS.contains("도구를 두 번 이상 호출할 작업"));
        assert!(CLAUDE_DEVEZ_INSTRUCTIONS.contains("동시에 `in_progress`인 Task는 하나만"));
        assert!(DEVEZ_INSTRUCTIONS.contains("같은 내용을 반복하지 않는다"));
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("클래스명, 메서드명, 변수명 등 기술 식별자"));
            assert!(rules.contains("꼭 필요한 경우가 아니면"));
            assert!(rules.contains("사용자 판단에 필요한 최소 범위만 쓴다"));
            assert!(rules.contains(
                "코드 변경 보고에서도 파일 경로와 핵심 코드는 사용자 판단에 꼭 필요한 경우에만"
            ));
        }
        // Verification reporting is left to the host's own honest-reporting rule.
        // Restating it here only competed with that wording.
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(!rules.contains("검증 결과"));
            assert!(!rules.contains("검증하지 못한"));
        }
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("첫 검색 결과나 단일 키워드에 의존하지 않는다"));
            assert!(
                rules.contains("찾지 못했다는 이유만으로 기능이나 코드가 없다고 단정하지 않는다")
            );
            assert!(rules.contains("현재 구현, 과거 문제의 원인, 추측을 구분"));
            assert!(rules.contains("직접적인 결론, 이를 뒷받침하는 핵심 근거"));
            assert!(rules.contains("`수정했습니다`, `확인했습니다`만으로 결과를 끝내지 않는다"));
            assert!(
                rules
                    .contains("실제 응답이나 오류를 받기 전 취소·거절·완료·원인을 단정하지 않는다")
            );
            assert!(rules.contains("필요한 질문을 일반 text로 다시 보여 주고"));
            assert!(rules.contains("결론 정리"));
            assert!(rules.contains("종료 직전에 여러 Task를 한꺼번에"));
        }
        // The rules kept naming the notice "진행 안내", so the model started
        // printing that very term as a heading; the ban has to say the term is
        // an instruction, not output. The vagueness rule pairs with Super Vibe:
        // with identifiers banned, answers drifted into "일부 수정했습니다".
        for rules in [DEVEZ_INSTRUCTIONS, CLAUDE_DEVEZ_INSTRUCTIONS] {
            assert!(rules.contains("라벨이나 머리글을 붙이지 않고"));
            assert!(rules.contains("그대로 출력할 문구가 아니다"));
            assert!(rules.contains("대상이 드러나지 않는 문장으로 얼버무리지 않는다"));
        }
    }

    #[test]
    fn new_thread_params_include_selected_response_length() {
        let params = new_thread_params("C:\\repo", None, None, "startup", "low", "default", "max");

        assert_eq!(
            params
                .pointer("/config/model_verbosity")
                .and_then(Value::as_str),
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
                model: "gpt-5.6-sol".to_owned(),
                skills: Err("skills offline".to_owned()),
                plugins: Err("plugins offline".to_owned()),
                apps: Err("apps offline".to_owned()),
                mcp: Err("mcp offline".to_owned()),
            },
        )
        .expect_err("all four catalogue requests failed");

        assert_eq!(
            error.to_string(),
            "Skill 조회 실패: skills offline; 플러그인 조회 실패: plugins offline; App 조회 실패: apps offline; MCP 조회 실패: mcp offline"
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
            let action = observe_key(&mut state, &mut buffer, press(code, KeyModifiers::NONE), at);
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
            observe_key(&mut state, &mut buffer, release, Instant::now()),
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
            observe_key(
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
            observe_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Char('답'), KeyModifiers::NONE),
                committed_at,
            ),
            Action::None
        ));
        let action = observe_key(
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

        observe_key(
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
            observe_key(
                &mut state,
                &mut buffer,
                press(KeyCode::Char('4'), KeyModifiers::NONE),
                selected_at,
            ),
            Action::None
        ));
        observe_key(
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
            observe_key(
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
        observe_key(
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
        observe_key(
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
            let action = observe_key(&mut state, &mut buffer, press(code, KeyModifiers::NONE), at);
            assert!(!matches!(action, Action::Submit(_)), "no key may submit");
        }

        assert_eq!(
            state.editor.paste_summary_lines(),
            None,
            "the block expanded"
        );
        // The buffer keeps the paste with its line endings normalized to LF.
        assert!(state.editor.text().starts_with(&pasted.replace("\r\n", "\n")));
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

        let action = observe_key(
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
            format!("{} Model", crate::state::UNCHECKED_BOX)
        );
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

    #[tokio::test]
    async fn a_slow_start_request_keeps_advancing_its_paint_clock() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let ticks = Arc::new(AtomicUsize::new(0));
        let wake = Arc::new(tokio::sync::Notify::new());
        let waiting_ticks = Arc::clone(&ticks);
        let waiting_wake = Arc::clone(&wake);
        let response = timeout(
            Duration::from_secs(1),
            await_with_ticks(
                async move {
                    while waiting_ticks.load(Ordering::SeqCst) < 3 {
                        waiting_wake.notified().await;
                    }
                    "ready"
                },
                Duration::from_millis(2),
                || {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    wake.notify_one();
                    Ok(())
                },
            ),
        )
        .await
        .expect("the paint clock did not advance")
        .unwrap();

        assert_eq!(response, "ready");
        assert!(
            ticks.load(Ordering::SeqCst) >= 3,
            "the wait kept repainting before the reply"
        );
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
        assert!(
            state
                .drain_committed()
                .iter()
                .all(|block| !matches!(block.kind, BlockKind::User))
        );
        assert_eq!(state.take_discarded_prompt_ids().len(), 1);
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

        assert!(
            hold_until_thread(
                &mut state,
                Action::SetTheme(theme::ThemeKind::Soft),
                &mut queued
            )
            .is_some()
        );
        assert!(hold_until_thread(&mut state, Action::Quit, &mut queued).is_some());
        assert!(hold_until_thread(&mut state, Action::Compact, &mut queued).is_none());
    }

    #[test]
    fn startup_keeps_composer_setting_clicks_until_the_thread_is_bound() {
        let mut state = starting_state();
        let mut queued = None;

        let (shell, diff) = state.cycle_vibe_mode();
        let vibe = Action::PersistVibeDisplayModes {
            vibe: state.vibe_mode(),
            response: state.response_length(),
            shell,
            diff,
        };
        assert!(hold_until_thread(&mut state, vibe, &mut queued).is_none());
        assert!(hold_until_thread(&mut state, Action::SetFast(true), &mut queued).is_none());

        let deferred = state.take_deferred_startup_actions();
        assert_eq!(deferred.len(), 2);
        assert!(
            deferred
                .iter()
                .any(|action| matches!(action, Action::PersistVibeDisplayModes { .. }))
        );
        assert!(
            deferred
                .iter()
                .any(|action| matches!(action, Action::SetFast(true)))
        );
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

    #[test]
    fn resume_list_uses_the_selected_provider() {
        assert_eq!(session_provider("gpt-5.6-sol"), "codex");
        assert_eq!(session_provider("claude:opus[1m]"), "claude");
        assert_eq!(session_provider("opencode:opencode/big-pickle"), "opencode");
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

    /// `/resume` keeps the previous transcript painted and names the wait until
    /// the replacement history is ready for one final screen swap.
    #[test]
    fn resume_wait_shows_a_loading_state_before_history_arrives() {
        let mut state = starting_state();
        state.attach_thread("thread-1".to_owned(), ".".to_owned(), "gpt-5.6-sol", None);
        state.handle_paste("old prompt");
        state.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));

        state.prepare_resume();
        state.begin_thread_switch();
        state.set_host_loading(true);

        assert!(state.thread_pending());
        assert!(!state.busy);
        assert!(state.drain_committed().is_empty(), "transcript was wiped");

        let view = state.view();
        assert!(view.welcome.is_none());
        assert!(view.status_line.is_some(), "the status line stays painted");
        assert_eq!(view.activity.as_deref(), Some("Loading session.."));
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

        assert_eq!(
            thread.pointer("/turns/0/id").and_then(Value::as_str),
            Some("turn-1")
        );
    }

    #[test]
    fn resume_history_pages_request_full_items_in_chronological_order() {
        let params = turns_list_params("thread-9", Some("cursor-100"));

        assert_eq!(
            params.pointer("/threadId").and_then(Value::as_str),
            Some("thread-9")
        );
        assert_eq!(
            params.pointer("/cursor").and_then(Value::as_str),
            Some("cursor-100")
        );
        assert_eq!(
            params.pointer("/sortDirection").and_then(Value::as_str),
            Some("asc")
        );
        assert_eq!(
            params.pointer("/itemsView").and_then(Value::as_str),
            Some("full")
        );

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

        assert_eq!(
            format_upgrade_result(&json!({ "message": "Claude marketplace updated" })),
            "Claude marketplace updated"
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
    fn management_writes_use_each_providers_native_protocol() {
        let (method, params) =
            mcp_toggle_request(SkillProvider::Claude, "claude-session", "browser", false);
        assert_eq!(method, "mcp/toggle");
        assert_eq!(params["sessionId"], "claude-session");
        assert_eq!(params["enabled"], false);

        let (method, params) = mcp_toggle_request(SkillProvider::Codex, "unused", "browser", true);
        assert_eq!(method, "config/value/write");
        assert_eq!(params["keyPath"], "mcp_servers.browser.enabled");
        assert_eq!(params["value"], true);

        let (method, params) =
            plugin_write_request("claude:sonnet", "browser@catalog", Some("project"), true);
        assert_eq!(method, "plugin/set-enabled");
        assert_eq!(params["scope"], "project");

        let (method, params) = skill_write_request(
            "C:/repo",
            SkillProvider::Codex,
            "C:/skills/review/SKILL.md",
            None,
            "user",
            false,
        )
        .expect("Codex skill request");
        assert_eq!(method, "skills/config/write");
        assert_eq!(params["path"], "C:/skills/review/SKILL.md");
        assert_eq!(params["enabled"], false);
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
            MouseRequest::Scroll(WHEEL_ROWS, 4, 7)
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
            MouseRequest::Scroll(WHEEL_ROWS, 4, 7)
        );
        assert_eq!(
            mouse_request(&at(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::NONE
            )),
            MouseRequest::None
        );
    }

}
