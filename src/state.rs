use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value, json};

use crate::{
    editor::Editor,
    renderer::{
        Block, BlockKind, ComposerMode, ModeAccent, OverlayLine, OverlayStyle, OverlayView,
        StatusLineView, SuggestionView, View, WelcomeView,
    },
};

const SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

/// The permission presets Codex exposes through `/permissions`, cycled with Shift+Tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionMode {
    ReadOnly,
    Default,
    FullAccess,
}

impl PermissionMode {
    const CYCLE: [Self; 3] = [Self::ReadOnly, Self::Default, Self::FullAccess];

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read Only",
            Self::Default => "Default",
            Self::FullAccess => "Full Access",
        }
    }

    /// Built-in permission profile id understood by the app-server.
    pub fn profile(self) -> &'static str {
        match self {
            Self::ReadOnly => ":read-only",
            Self::Default => ":workspace",
            Self::FullAccess => ":danger-full-access",
        }
    }

    fn accent(self) -> ModeAccent {
        match self {
            Self::ReadOnly => ModeAccent::Calm,
            Self::Default => ModeAccent::Safe,
            Self::FullAccess => ModeAccent::Danger,
        }
    }

    fn next(self) -> Self {
        let index = Self::CYCLE
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::CYCLE[(index + 1) % Self::CYCLE.len()]
    }
}

struct SlashCommand {
    name: &'static str,
    description: &'static str,
    takes_argument: bool,
}

const SLASH_COMMANDS: [SlashCommand; 17] = [
    SlashCommand {
        name: "/model",
        description: "Switch model and reasoning",
        takes_argument: true,
    },
    SlashCommand {
        name: "/fast",
        description: "Toggle the model's fast service tier",
        takes_argument: false,
    },
    SlashCommand {
        name: "/effort",
        description: "Set reasoning effort",
        takes_argument: true,
    },
    SlashCommand {
        name: "/new",
        description: "Start a new thread",
        takes_argument: false,
    },
    SlashCommand {
        name: "/resume",
        description: "Resume a saved session",
        takes_argument: true,
    },
    SlashCommand {
        name: "/continue",
        description: "Alias for /resume",
        takes_argument: false,
    },
    SlashCommand {
        name: "/btw",
        description: "Start an ephemeral side conversation",
        takes_argument: true,
    },
    SlashCommand {
        name: "/side",
        description: "Alias for /btw",
        takes_argument: true,
    },
    SlashCommand {
        name: "/compact",
        description: "Compact the current conversation",
        takes_argument: false,
    },
    SlashCommand {
        name: "/copy",
        description: "Copy the last response as Markdown",
        takes_argument: false,
    },
    SlashCommand {
        name: "/diff",
        description: "Show the current git diff",
        takes_argument: false,
    },
    SlashCommand {
        name: "/usage",
        description: "Show account usage limits",
        takes_argument: false,
    },
    SlashCommand {
        name: "/status",
        description: "Show session details",
        takes_argument: false,
    },
    SlashCommand {
        name: "/clear",
        description: "Clear the terminal",
        takes_argument: false,
    },
    SlashCommand {
        name: "/help",
        description: "Show commands and shortcuts",
        takes_argument: false,
    },
    SlashCommand {
        name: "/quit",
        description: "Exit Devez CLI",
        takes_argument: false,
    },
    SlashCommand {
        name: "/exit",
        description: "Alias for /quit",
        takes_argument: false,
    },
];

#[derive(Clone)]
pub struct ModelInfo {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub efforts: Vec<EffortInfo>,
    pub default_effort: String,
    pub is_default: bool,
    pub context_window: Option<u64>,
    pub fast_service_tier: Option<String>,
}

#[derive(Clone)]
pub struct EffortInfo {
    pub id: String,
}

impl ModelInfo {
    pub fn from_value(value: &Value) -> Option<Self> {
        let efforts = value
            .get("supportedReasoningEfforts")?
            .as_array()?
            .iter()
            .filter_map(|entry| {
                Some(EffortInfo {
                    id: entry.get("reasoningEffort")?.as_str()?.to_owned(),
                })
            })
            .collect::<Vec<_>>();
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            model: value.get("model")?.as_str()?.to_owned(),
            display_name: value.get("displayName")?.as_str()?.to_owned(),
            default_effort: value.get("defaultReasoningEffort")?.as_str()?.to_owned(),
            efforts,
            is_default: value
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            context_window: value.get("contextWindow").and_then(Value::as_u64),
            fast_service_tier: value
                .get("serviceTiers")
                .and_then(Value::as_array)
                .and_then(|tiers| {
                    tiers.iter().find_map(|tier| {
                        let is_fast = tier
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| name.eq_ignore_ascii_case("fast"));
                        is_fast
                            .then(|| {
                                tier.get("id")
                                    .and_then(Value::as_str)
                                    .map(ToOwned::to_owned)
                            })
                            .flatten()
                    })
                }),
        })
    }

    pub fn supports_effort(&self, effort: &str) -> bool {
        self.efforts.iter().any(|candidate| candidate.id == effort)
    }

    pub fn matches_query(&self, query: &str) -> bool {
        if self.id.eq_ignore_ascii_case(query)
            || self.model.eq_ignore_ascii_case(query)
            || self.display_name.eq_ignore_ascii_case(query)
        {
            return true;
        }

        let query = query.trim().to_ascii_lowercase();
        let identity = format!("{} {}", self.model, self.display_name).to_ascii_lowercase();
        match query.as_str() {
            "sol" => identity.contains("5.6") && identity.contains("sol"),
            "terra" => identity.contains("5.6") && identity.contains("terra"),
            "luna" => identity.contains("5.6") && identity.contains("luna"),
            "5.5" => identity.contains("5.5"),
            "5.4" => identity.contains("5.4") && !identity.contains("mini"),
            "mini" | "5.4-mini" => identity.contains("5.4") && identity.contains("mini"),
            "spark" | "5.3" => identity.contains("5.3") && identity.contains("spark"),
            _ => false,
        }
    }
}

pub enum Action {
    None,
    Submit(String),
    Steer(String),
    Interrupt,
    NewThread,
    OpenResume,
    ResumeThread(String),
    SetFast(bool),
    StartSide(Option<String>),
    ReturnFromSide,
    Compact,
    Copy(String),
    ShowDiff,
    Quit,
    ClearScreen,
    Tick(bool),
    RpcResponse { id: Value, result: Value },
    RpcError { id: Value, message: String },
}

struct SideParent {
    thread_id: String,
}

struct ActiveItem {
    block: Block,
}

enum PendingInteraction {
    ModelPicker {
        model_index: usize,
        effort_index: usize,
    },
    EffortPicker {
        effort_index: usize,
    },
    SessionPicker(SessionPicker),
    Approval {
        id: Value,
        title: String,
        detail: Vec<String>,
        once: Value,
        session: Option<Value>,
        decline: Value,
    },
    UserInput {
        id: Value,
        questions: Vec<Question>,
        current: usize,
        selected: usize,
        text_mode: bool,
        editor: Editor,
        answers: BTreeMap<String, String>,
    },
}

struct Question {
    id: String,
    header: String,
    question: String,
    options: Vec<QuestionOption>,
    allow_other: bool,
}

struct QuestionOption {
    label: String,
    description: String,
}

#[derive(Clone)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub preview: String,
    pub cwd: String,
    pub updated_at: u64,
}

impl SessionInfo {
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            preview: value
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or("Untitled session")
                .lines()
                .next()
                .unwrap_or("Untitled session")
                .to_owned(),
            cwd: value
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            updated_at: value
                .get("updatedAt")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        })
    }

    fn title(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.preview)
    }
}

pub enum SessionPickerResult {
    None,
    Cancel,
    Select(String),
}

pub struct SessionPicker {
    sessions: Vec<SessionInfo>,
    cwd: String,
    current_thread_id: Option<String>,
    selected: usize,
    all_projects: bool,
    query: Editor,
}

impl SessionPicker {
    pub fn new(sessions: Vec<SessionInfo>, cwd: String, current_thread_id: Option<String>) -> Self {
        Self {
            sessions,
            cwd,
            current_thread_id,
            selected: 0,
            all_projects: false,
            query: Editor::default(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SessionPickerResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return SessionPickerResult::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => SessionPickerResult::Cancel,
            KeyCode::Char('c') if ctrl => SessionPickerResult::Cancel,
            KeyCode::Char('a') if ctrl => {
                self.all_projects = !self.all_projects;
                self.selected = 0;
                SessionPickerResult::None
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.selected = 0;
                SessionPickerResult::None
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                SessionPickerResult::None
            }
            KeyCode::Char('p') if ctrl => {
                self.selected = self.selected.saturating_sub(1);
                SessionPickerResult::None
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.filtered_len().saturating_sub(1));
                SessionPickerResult::None
            }
            KeyCode::Char('n') if ctrl => {
                self.selected = (self.selected + 1).min(self.filtered_len().saturating_sub(1));
                SessionPickerResult::None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                SessionPickerResult::None
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 8).min(self.filtered_len().saturating_sub(1));
                SessionPickerResult::None
            }
            KeyCode::Enter => self
                .filtered()
                .get(self.selected)
                .map(|session| SessionPickerResult::Select(session.id.clone()))
                .unwrap_or(SessionPickerResult::None),
            KeyCode::Backspace if ctrl => {
                self.query.delete_word_left();
                self.selected = 0;
                SessionPickerResult::None
            }
            KeyCode::Backspace => {
                self.query.backspace();
                self.selected = 0;
                SessionPickerResult::None
            }
            KeyCode::Delete => {
                self.query.delete();
                self.selected = 0;
                SessionPickerResult::None
            }
            KeyCode::Left => {
                self.query.move_left();
                SessionPickerResult::None
            }
            KeyCode::Right => {
                self.query.move_right();
                SessionPickerResult::None
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.move_word_left();
                SessionPickerResult::None
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.move_word_right();
                SessionPickerResult::None
            }
            KeyCode::Home => {
                self.query.move_home();
                SessionPickerResult::None
            }
            KeyCode::End => {
                self.query.move_end();
                SessionPickerResult::None
            }
            KeyCode::Char(ch) if !ctrl => {
                self.query.insert(ch);
                self.selected = 0;
                SessionPickerResult::None
            }
            _ => SessionPickerResult::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.query.insert_str(text);
        self.selected = 0;
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        let filtered = self.filtered();
        let start = self.selected.saturating_sub(4);
        let end = (start + 9).min(filtered.len());
        let mut lines = filtered[start..end]
            .iter()
            .enumerate()
            .map(|(offset, session)| {
                let index = start + offset;
                let current = self
                    .current_thread_id
                    .as_deref()
                    .is_some_and(|id| id == session.id);
                let path = if self.all_projects {
                    format!("\n      {}", session.cwd)
                } else {
                    String::new()
                };
                OverlayLine {
                    text: format!(
                        "{}  ·  {}{}{}",
                        session.title(),
                        relative_time(session.updated_at),
                        if current { "  ·  current" } else { "" },
                        path
                    ),
                    selected: index == self.selected,
                    muted: false,
                }
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(OverlayLine {
                text: if self.query.is_empty() {
                    "No sessions found in this folder.".to_owned()
                } else {
                    "No sessions match your search.".to_owned()
                },
                selected: false,
                muted: true,
            });
        }
        OverlayView {
            title: format!(
                "Resume session · {} · {}",
                filtered.len(),
                if self.all_projects {
                    "all projects"
                } else {
                    "this folder"
                }
            ),
            lines,
            hint: "↑↓ navigate  Enter resume  Ctrl+A all projects  Esc cancel".to_owned(),
            style: OverlayStyle::Panel,
            input: Some(&self.query),
            input_label: "Search",
            input_placeholder: "Search by name, prompt, ID, or folder…",
        }
    }

    fn filtered(&self) -> Vec<&SessionInfo> {
        let query = self.query.text().to_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                (self.all_projects || path_eq(&session.cwd, &self.cwd))
                    && (query.is_empty()
                        || session.title().to_lowercase().contains(&query)
                        || session.id.to_lowercase().contains(&query)
                        || session.cwd.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn filtered_len(&self) -> usize {
        self.filtered().len()
    }
}

pub struct AppState {
    pub editor: Editor,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub busy: bool,
    pub cwd: String,
    account: String,
    models: Vec<ModelInfo>,
    selected_model: usize,
    selected_effort: String,
    committed: Vec<Block>,
    active_order: Vec<String>,
    active: HashMap<String, ActiveItem>,
    pending: Option<PendingInteraction>,
    total_tokens: u64,
    context_window: Option<u64>,
    transient_status: Option<String>,
    show_welcome: bool,
    command_selection: usize,
    spinner_frame: usize,
    turn_started_at: Option<Instant>,
    branch: Option<String>,
    five_hour_percent: Option<u8>,
    weekly_percent: Option<u8>,
    fast_mode: bool,
    side_parent: Option<SideParent>,
    last_assistant_markdown: Option<String>,
    composer_notice: Option<(String, Instant)>,
    status_metadata_refreshed_at: Instant,
    permission_mode: PermissionMode,
}

impl AppState {
    pub fn new(
        thread_id: String,
        cwd: String,
        account: String,
        models: Vec<ModelInfo>,
        model: &str,
        effort: Option<&str>,
    ) -> Self {
        let selected_model = models
            .iter()
            .position(|candidate| candidate.id == model || candidate.model == model)
            .or_else(|| models.iter().position(|candidate| candidate.is_default))
            .unwrap_or(0);
        let selected_effort = models
            .get(selected_model)
            .map(|selected| {
                effort
                    .filter(|effort| selected.supports_effort(effort))
                    .unwrap_or(&selected.default_effort)
                    .to_owned()
            })
            .or_else(|| effort.map(ToOwned::to_owned))
            .unwrap_or_else(|| "high".to_owned());
        let branch = read_git_branch(&cwd);
        let (five_hour_percent, weekly_percent) = read_codex_usage();
        let context_window = models
            .get(selected_model)
            .and_then(|model| model.context_window);

        Self {
            editor: Editor::default(),
            thread_id,
            turn_id: None,
            busy: false,
            cwd,
            account,
            models,
            selected_model,
            selected_effort,
            committed: Vec::new(),
            active_order: Vec::new(),
            active: HashMap::new(),
            pending: None,
            total_tokens: 0,
            context_window,
            transient_status: None,
            show_welcome: true,
            command_selection: 0,
            spinner_frame: 0,
            turn_started_at: None,
            branch,
            five_hour_percent,
            weekly_percent,
            fast_mode: read_fast_mode(),
            side_parent: None,
            last_assistant_markdown: None,
            composer_notice: None,
            status_metadata_refreshed_at: Instant::now(),
            permission_mode: read_permission_mode(),
        }
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.models.get(self.selected_model)
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
    }

    /// Permission profile id to send with `turn/start`.
    pub fn permission_profile(&self) -> &'static str {
        self.permission_mode().profile()
    }

    fn composer_mode(&self) -> ComposerMode {
        ComposerMode {
            label: self.permission_mode().label().to_owned(),
            accent: self.permission_mode().accent(),
        }
    }

    pub fn selected_model_name(&self) -> &str {
        self.selected_model()
            .map(|model| model.model.as_str())
            .unwrap_or("default")
    }

    pub fn selected_model_display_name(&self) -> &str {
        self.selected_model()
            .map(|model| model.display_name.as_str())
            .unwrap_or_else(|| self.selected_model_name())
    }

    pub fn selected_effort(&self) -> &str {
        &self.selected_effort
    }

    pub fn service_tier(&self) -> &str {
        if self.effective_fast_mode() {
            self.selected_model()
                .and_then(|model| model.fast_service_tier.as_deref())
                .unwrap_or("priority")
        } else {
            "default"
        }
    }

    pub fn effective_fast_mode(&self) -> bool {
        self.fast_mode
            && self
                .selected_model()
                .is_some_and(|model| model.fast_service_tier.is_some())
    }

    pub fn set_fast_mode(&mut self, enabled: bool) {
        self.fast_mode = enabled;
    }

    pub fn set_copy_notice(&mut self, count: usize) {
        self.composer_notice = Some((format!("Copied {count} chars to clipboard"), Instant::now()));
    }

    pub fn side_parent_thread_id(&self) -> Option<&str> {
        self.side_parent
            .as_ref()
            .map(|parent| parent.thread_id.as_str())
    }

    pub fn enter_side_thread(
        &mut self,
        thread_id: String,
        cwd: String,
        model: &str,
        effort: Option<&str>,
    ) {
        let parent = SideParent {
            thread_id: self.thread_id.clone(),
        };
        self.prepare_resume();
        self.side_parent = Some(parent);
        self.set_thread(thread_id, cwd, model, effort);
        self.show_welcome = false;
        self.transient_status = Some("Side · Ctrl+C to return".to_owned());
        self.committed.push(Block::new(
            BlockKind::System,
            "Side conversation",
            "Ephemeral fork · Ctrl+C to return to the main thread",
        ));
    }

    pub fn begin_side_prompt(&mut self, text: String) {
        self.show_welcome = false;
        self.committed
            .push(Block::new(BlockKind::User, "You", text));
        self.busy = true;
    }

    pub fn set_thread(
        &mut self,
        thread_id: String,
        cwd: String,
        model: &str,
        effort: Option<&str>,
    ) {
        self.thread_id = thread_id;
        self.cwd = cwd;
        self.branch = read_git_branch(&self.cwd);
        self.turn_id = None;
        self.busy = false;
        self.turn_started_at = None;
        self.active.clear();
        self.active_order.clear();
        self.show_welcome = true;
        if let Some(index) = self
            .models
            .iter()
            .position(|candidate| candidate.id == model || candidate.model == model)
        {
            self.selected_model = index;
        }
        self.context_window = self.selected_model().and_then(|model| model.context_window);
        self.selected_effort = self
            .selected_model()
            .map(|model| {
                effort
                    .filter(|effort| model.supports_effort(effort))
                    .unwrap_or(&model.default_effort)
                    .to_owned()
            })
            .or_else(|| effort.map(ToOwned::to_owned))
            .unwrap_or_else(|| self.selected_effort.clone());
    }

    pub fn load_history(&mut self, thread: &Value) {
        let Some(turns) = thread.get("turns").and_then(Value::as_array) else {
            return;
        };
        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if let Some(block) = completed_item_block(item) {
                    if matches!(block.kind, BlockKind::Assistant) {
                        self.last_assistant_markdown = Some(block.body.clone());
                    }
                    self.committed.push(block);
                }
            }
        }
        self.show_welcome = false;
    }

    pub fn set_turn_started(&mut self, turn_id: String) {
        self.turn_id = Some(turn_id);
        self.busy = true;
        self.turn_started_at = Some(Instant::now());
    }

    pub fn set_request_failed(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.turn_id = None;
        self.turn_started_at = None;
        self.committed
            .push(Block::new(BlockKind::Error, "요청 실패", message));
    }

    pub fn open_session_picker(&mut self, sessions: Vec<SessionInfo>) {
        self.pending = Some(PendingInteraction::SessionPicker(SessionPicker::new(
            sessions,
            self.cwd.clone(),
            Some(self.thread_id.clone()),
        )));
    }

    pub fn prepare_resume(&mut self) {
        self.committed.clear();
        self.active.clear();
        self.active_order.clear();
        self.pending = None;
        self.total_tokens = 0;
        self.context_window = None;
        self.transient_status = None;
        self.side_parent = None;
        self.last_assistant_markdown = None;
        self.composer_notice = None;
        self.show_welcome = false;
        self.busy = false;
        self.turn_id = None;
        self.turn_started_at = None;
    }

    pub fn prepare_new_thread(&mut self) {
        self.prepare_resume();
        self.editor.clear();
        self.show_welcome = true;
    }

    pub fn push_notice(
        &mut self,
        kind: BlockKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.committed.push(Block::new(kind, title, body));
    }

    pub fn drain_committed(&mut self) -> Vec<Block> {
        if self.show_welcome && !self.committed.is_empty() {
            let pending = std::mem::take(&mut self.committed);
            self.commit_welcome_card();
            self.committed.extend(pending);
        }
        std::mem::take(&mut self.committed)
    }

    pub fn view(&self) -> View<'_> {
        let live_blocks = self
            .active_order
            .iter()
            .filter_map(|id| self.active.get(id))
            .map(|item| item.block.clone())
            .collect::<Vec<_>>();
        View {
            live_blocks,
            overlay: self.overlay_view(),
            editor: &self.editor,
            welcome: self.show_welcome.then(|| WelcomeView {
                model: self.selected_model_display_name().to_owned(),
                effort: self.selected_effort.clone(),
                cwd: self.cwd.clone(),
                account: self.account.clone(),
            }),
            suggestions: if self.pending.is_none() {
                self.slash_suggestion_views()
            } else {
                Vec::new()
            },
            activity: self.activity(),
            footer: String::new(),
            status_line: Some(self.status_line()),
            composer_notice: self
                .composer_notice
                .as_ref()
                .map(|(notice, _)| notice.clone()),
            composer_mode: Some(self.composer_mode()),
        }
    }

    pub fn tick(&mut self) -> bool {
        let mut redraw = self.busy;
        if self.busy {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
        }
        if self.status_metadata_refreshed_at.elapsed().as_secs() >= 3 {
            self.branch = read_git_branch(&self.cwd);
            (self.five_hour_percent, self.weekly_percent) = read_codex_usage();
            self.fast_mode = read_fast_mode();
            self.status_metadata_refreshed_at = Instant::now();
        }
        if self
            .composer_notice
            .as_ref()
            .is_some_and(|(_, shown_at)| shown_at.elapsed().as_millis() >= 1_400)
        {
            self.composer_notice = None;
            redraw = true;
        }
        redraw
    }

    pub fn handle_paste(&mut self, text: &str) {
        match &mut self.pending {
            Some(PendingInteraction::UserInput {
                text_mode: true,
                editor,
                ..
            }) => editor.insert_str(text),
            Some(PendingInteraction::SessionPicker(picker)) => picker.handle_paste(text),
            Some(_) => {}
            None => {
                self.editor.insert_str(text);
                self.command_selection = 0;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if self.pending.is_some() {
            return self.handle_pending_key(key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        let slash_matches = self.matching_slash_commands();
        if !slash_matches.is_empty() && ctrl {
            match key.code {
                KeyCode::Char('p') => {
                    self.command_selection = self.command_selection.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Char('n') => {
                    self.command_selection =
                        (self.command_selection + 1).min(slash_matches.len() - 1);
                    return Action::None;
                }
                _ => {}
            }
        }
        if !slash_matches.is_empty() && !ctrl && !alt && !shift {
            match key.code {
                KeyCode::Up => {
                    self.command_selection = self.command_selection.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.command_selection =
                        (self.command_selection + 1).min(slash_matches.len() - 1);
                    return Action::None;
                }
                KeyCode::Tab => {
                    let selected =
                        slash_matches[self.command_selection.min(slash_matches.len() - 1)];
                    self.editor.set_text(if selected.takes_argument {
                        format!("{} ", selected.name)
                    } else {
                        selected.name.to_owned()
                    });
                    self.command_selection = 0;
                    return Action::None;
                }
                KeyCode::Enter => {
                    let selected =
                        slash_matches[self.command_selection.min(slash_matches.len() - 1)];
                    self.editor.set_text(selected.name);
                    self.command_selection = 0;
                    return self.submit_editor();
                }
                _ => {}
            }
        }

        match key.code {
            // Shift+Tab arrives as BackTab on terminals without the Kitty keyboard protocol.
            KeyCode::BackTab => {
                self.cycle_permission_mode();
                Action::None
            }
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    Action::Interrupt
                } else if self.editor.is_empty() && self.side_parent.is_some() {
                    Action::ReturnFromSide
                } else if self.editor.is_empty() {
                    Action::Quit
                } else {
                    self.editor.clear();
                    Action::None
                }
            }
            KeyCode::Char('d') if ctrl && self.editor.is_empty() && !self.busy => Action::Quit,
            KeyCode::Char('d') if ctrl => {
                self.editor.delete();
                Action::None
            }
            KeyCode::Char('l') if ctrl => Action::ClearScreen,
            KeyCode::Char('a') if ctrl => {
                self.editor.move_home();
                Action::None
            }
            KeyCode::Char('e') if ctrl => {
                self.editor.move_end();
                Action::None
            }
            KeyCode::Char('w') if ctrl => {
                self.editor.delete_word_left();
                Action::None
            }
            KeyCode::Char('k') if ctrl => {
                self.editor.delete_to_line_end();
                Action::None
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.delete_to_line_start();
                Action::None
            }
            KeyCode::Char('y') if ctrl => {
                self.editor.yank();
                Action::None
            }
            KeyCode::Char('j') if ctrl => {
                self.editor.newline();
                Action::None
            }
            KeyCode::Char('b') if alt => {
                self.editor.move_word_left();
                Action::None
            }
            KeyCode::Char('f') if alt => {
                self.editor.move_word_right();
                Action::None
            }
            KeyCode::Enter if alt || shift || ctrl => {
                self.editor.newline();
                Action::None
            }
            KeyCode::Enter => self.submit_editor(),
            KeyCode::Esc if self.busy => Action::Interrupt,
            KeyCode::Backspace if ctrl => {
                self.editor.delete_word_left();
                self.command_selection = 0;
                Action::None
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                self.command_selection = 0;
                Action::None
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.command_selection = 0;
                Action::None
            }
            KeyCode::Left if alt || ctrl => {
                self.editor.move_word_left();
                Action::None
            }
            KeyCode::Right if alt || ctrl => {
                self.editor.move_word_right();
                Action::None
            }
            KeyCode::Left => {
                self.editor.move_left();
                Action::None
            }
            KeyCode::Right => {
                self.editor.move_right();
                Action::None
            }
            KeyCode::Home => {
                self.editor.move_home();
                Action::None
            }
            KeyCode::End => {
                self.editor.move_end();
                Action::None
            }
            KeyCode::Up => {
                self.editor.history_previous();
                Action::None
            }
            KeyCode::Down => {
                self.editor.history_next();
                Action::None
            }
            KeyCode::Char(ch) if !ctrl => {
                self.editor.insert(ch);
                self.command_selection = 0;
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn begin_server_request(&mut self, id: Value, method: &str, params: &Value) -> Action {
        if self.pending.is_some() {
            return Action::RpcError {
                id,
                message: "다른 사용자 입력을 처리 중입니다.".to_owned(),
            };
        }

        match method {
            "item/commandExecution/requestApproval" => {
                let command = params
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("명령 실행");
                let mut detail = vec![command.to_owned()];
                if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                    detail.push(format!("위치: {cwd}"));
                }
                if let Some(reason) = params.get("reason").and_then(Value::as_str) {
                    detail.push(format!("이유: {reason}"));
                }
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: "명령 실행을 허용할까요?".to_owned(),
                    detail,
                    once: json!({ "decision": "accept" }),
                    session: Some(json!({ "decision": "acceptForSession" })),
                    decline: json!({ "decision": "decline" }),
                });
                Action::None
            }
            "item/fileChange/requestApproval" => {
                let mut detail = Vec::new();
                if let Some(reason) = params.get("reason").and_then(Value::as_str) {
                    detail.push(reason.to_owned());
                }
                if let Some(root) = params.get("grantRoot").and_then(Value::as_str) {
                    detail.push(format!("쓰기 경로: {root}"));
                }
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: "파일 변경을 허용할까요?".to_owned(),
                    detail,
                    once: json!({ "decision": "accept" }),
                    session: Some(json!({ "decision": "acceptForSession" })),
                    decline: json!({ "decision": "decline" }),
                });
                Action::None
            }
            "item/permissions/requestApproval" => {
                let requested = params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let detail = permission_detail(&requested);
                self.pending = Some(PendingInteraction::Approval {
                    id,
                    title: "추가 권한을 허용할까요?".to_owned(),
                    detail,
                    once: json!({ "permissions": requested, "scope": "turn" }),
                    session: Some(json!({
                        "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                        "scope": "session"
                    })),
                    decline: json!({ "permissions": {}, "scope": "turn" }),
                });
                Action::None
            }
            "item/tool/requestUserInput" => {
                let questions = parse_questions(params);
                if questions.is_empty() {
                    return Action::RpcResponse {
                        id,
                        result: json!({ "answers": {} }),
                    };
                }
                let text_mode = questions[0].options.is_empty();
                self.pending = Some(PendingInteraction::UserInput {
                    id,
                    questions,
                    current: 0,
                    selected: 0,
                    text_mode,
                    editor: Editor::default(),
                    answers: BTreeMap::new(),
                });
                Action::None
            }
            "mcpServer/elicitation/request" => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "MCP 입력 요청",
                    "이 초기 버전은 MCP 폼 입력을 아직 지원하지 않아 요청을 취소했습니다.",
                ));
                Action::RpcResponse {
                    id,
                    result: json!({ "action": "cancel", "content": null, "_meta": null }),
                }
            }
            _ => Action::RpcError {
                id,
                message: format!("지원하지 않는 서버 요청: {method}"),
            },
        }
    }

    pub fn handle_notification(&mut self, method: &str, params: &Value) {
        if params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|thread_id| thread_id != self.thread_id)
        {
            return;
        }
        match method {
            "turn/started" => {
                if let Some(turn_id) = params
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                {
                    self.set_turn_started(turn_id.to_owned());
                }
            }
            "turn/completed" => {
                self.busy = false;
                self.turn_id = None;
                self.turn_started_at = None;
                if let Some(error) = params
                    .get("turn")
                    .and_then(|turn| turn.get("error"))
                    .filter(|error| !error.is_null())
                {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "Turn 실패",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("알 수 없는 오류"),
                    ));
                }
                self.flush_orphaned_active();
            }
            "item/started" => {
                if let Some(item) = params.get("item") {
                    self.start_item(item);
                }
            }
            "item/completed" => {
                if let Some(item) = params.get("item") {
                    self.complete_item(item);
                }
            }
            "item/agentMessage/delta" => {
                self.append_delta(params, BlockKind::Assistant, "Codex");
            }
            "item/reasoning/summaryTextDelta" => {
                self.append_delta(params, BlockKind::Reasoning, "Thinking…");
            }
            "item/commandExecution/outputDelta" => {
                self.append_delta(params, BlockKind::Tool, "Command");
            }
            "item/plan/delta" => {
                self.append_delta(params, BlockKind::Reasoning, "Plan");
            }
            "item/fileChange/patchUpdated" => {
                if let Some(item_id) = params.get("itemId").and_then(Value::as_str) {
                    let body = file_changes_body(
                        params
                            .get("changes")
                            .and_then(Value::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                    );
                    self.ensure_active(item_id, BlockKind::Tool, "Files")
                        .block
                        .body = body;
                }
            }
            "item/mcpToolCall/progress" => {
                if let Some(item_id) = params.get("itemId").and_then(Value::as_str)
                    && let Some(message) = params.get("message").and_then(Value::as_str)
                {
                    append_capped(
                        &mut self
                            .ensure_active(item_id, BlockKind::Tool, "MCP")
                            .block
                            .body,
                        message,
                    );
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(usage) = params.get("tokenUsage") {
                    self.total_tokens = usage
                        .get("total")
                        .and_then(|total| total.get("totalTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(self.total_tokens);
                    self.context_window = usage
                        .get("modelContextWindow")
                        .and_then(Value::as_u64)
                        .or(self.context_window);
                }
            }
            "error" => {
                let message = params
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("알 수 없는 Codex 오류");
                let retry = params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.committed.push(Block::new(
                    if retry {
                        BlockKind::Warning
                    } else {
                        BlockKind::Error
                    },
                    if retry {
                        "재시도 중"
                    } else {
                        "Codex 오류"
                    },
                    message,
                ));
            }
            "warning" | "configWarning" | "guardianWarning" | "deprecationNotice" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| params.as_str().unwrap_or("Codex 경고"));
                self.committed
                    .push(Block::new(BlockKind::Warning, "경고", message));
            }
            "model/rerouted" => {
                if let (Some(from), Some(to)) = (
                    params.get("fromModel").and_then(Value::as_str),
                    params.get("toModel").and_then(Value::as_str),
                ) {
                    self.transient_status = Some(format!("{from} → {to}로 전환됨"));
                }
            }
            "thread/compacted" => self.committed.push(Block::new(
                BlockKind::System,
                "Context compacted",
                "대화 컨텍스트가 압축되었습니다.",
            )),
            _ => {}
        }
    }

    fn submit_editor(&mut self) -> Action {
        let Some(text) = self.editor.take_for_submit() else {
            return Action::None;
        };
        if text.starts_with('/') && !text.contains('\n') {
            return self.run_slash_command(&text);
        }
        self.commit_welcome_card();
        self.committed
            .push(Block::new(BlockKind::User, "You", text.clone()));
        if self.busy {
            Action::Steer(text)
        } else {
            self.busy = true;
            Action::Submit(text)
        }
    }

    fn run_slash_command(&mut self, command: &str) -> Action {
        let parts = command.split_whitespace().collect::<Vec<_>>();
        match parts.first().copied().unwrap_or_default() {
            "/help" => {
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Commands",
                    "/model [MODEL] [EFFORT]  모델과 effort 선택\n/fast  빠른 서비스 티어 전환\n/effort [LEVEL]  추론 수준\n/btw [MESSAGE]  임시 사이드 대화\n/compact  컨텍스트 압축\n/copy  마지막 답변 복사\n/diff  git diff 표시\n/resume [SESSION]  이전 세션 선택\n/continue  /resume 별칭\n/new  새 대화\n/status  현재 설정\n/usage  사용 한도\n/clear  화면 정리\n/quit  종료\n\nEsc 또는 Ctrl+C  실행 중단\nShift+Tab  권한 모드 전환 (Read Only / Default / Full Access)\nCtrl+Enter / Shift+Enter  줄바꿈",
                ));
                Action::None
            }
            "/fast" => {
                if self
                    .selected_model()
                    .is_none_or(|model| model.fast_service_tier.is_none())
                {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "Fast mode unavailable",
                        "현재 모델은 Fast 서비스 티어를 지원하지 않습니다.",
                    ));
                    Action::None
                } else {
                    let enabled = match parts.get(1).map(|value| value.to_ascii_lowercase()) {
                        Some(value) if value == "on" => true,
                        Some(value) if value == "off" => false,
                        Some(_) => {
                            self.committed.push(Block::new(
                                BlockKind::Error,
                                "Usage",
                                "/fast [on|off]",
                            ));
                            return Action::None;
                        }
                        None => !self.effective_fast_mode(),
                    };
                    Action::SetFast(enabled)
                }
            }
            "/model" if parts.len() == 1 => {
                let effort_index = self
                    .selected_model()
                    .and_then(|model| {
                        model
                            .efforts
                            .iter()
                            .position(|effort| effort.id == self.selected_effort)
                    })
                    .unwrap_or(0);
                self.pending = Some(PendingInteraction::ModelPicker {
                    model_index: self.selected_model,
                    effort_index,
                });
                Action::None
            }
            "/model" => {
                let query = parts[1];
                let index = query
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .filter(|index| *index < self.models.len())
                    .or_else(|| {
                        self.models
                            .iter()
                            .position(|candidate| candidate.matches_query(query))
                    });
                let Some(index) = index else {
                    self.committed
                        .push(Block::new(BlockKind::Error, "모델을 찾을 수 없음", query));
                    return Action::None;
                };
                let effort = parts.get(2).copied();
                self.apply_model(index, effort);
                Action::None
            }
            "/effort" if parts.len() == 1 => {
                let effort_index = self
                    .selected_model()
                    .and_then(|model| {
                        model
                            .efforts
                            .iter()
                            .position(|effort| effort.id == self.selected_effort)
                    })
                    .unwrap_or(0);
                self.pending = Some(PendingInteraction::EffortPicker { effort_index });
                Action::None
            }
            "/effort" if parts.len() == 2 => {
                let effort = parts[1];
                if self
                    .selected_model()
                    .is_some_and(|model| model.supports_effort(effort))
                {
                    self.apply_effort(effort);
                } else {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "지원하지 않는 reasoning effort",
                        effort,
                    ));
                }
                Action::None
            }
            "/resume" | "/continue" if self.busy => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "진행 중",
                    "현재 응답을 중단한 뒤 세션을 전환하세요.",
                ));
                Action::None
            }
            "/resume" if parts.len() == 1 => Action::OpenResume,
            "/resume" => Action::ResumeThread(parts[1..].join(" ")),
            "/continue" => Action::OpenResume,
            "/btw" | "/side" if self.busy => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "진행 중",
                    "현재 응답을 중단한 뒤 사이드 대화를 시작하세요.",
                ));
                Action::None
            }
            "/btw" | "/side" => Action::StartSide((parts.len() > 1).then(|| parts[1..].join(" "))),
            "/compact" if self.busy => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "진행 중",
                    "현재 응답이 끝난 뒤 컨텍스트를 압축하세요.",
                ));
                Action::None
            }
            "/compact" => Action::Compact,
            "/copy" => match self.last_assistant_markdown.clone() {
                Some(markdown) => Action::Copy(markdown),
                None => {
                    self.committed.push(Block::new(
                        BlockKind::Warning,
                        "Nothing to copy",
                        "완료된 답변이 아직 없습니다.",
                    ));
                    Action::None
                }
            },
            "/diff" => Action::ShowDiff,
            "/new" if self.busy => {
                self.committed.push(Block::new(
                    BlockKind::Warning,
                    "진행 중",
                    "현재 응답을 중단한 뒤 새 대화를 시작하세요.",
                ));
                Action::None
            }
            "/new" => Action::NewThread,
            "/status" => {
                let model = self.selected_model_display_name();
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Status",
                    format!(
                        "thread: {}\nmodel: {model}\neffort: {}\npermissions: {} ({})\ncwd: {}",
                        self.thread_id,
                        self.selected_effort,
                        self.permission_mode.label(),
                        self.permission_mode.profile(),
                        self.cwd
                    ),
                ));
                Action::None
            }
            "/usage" => {
                let five_hour = self
                    .five_hour_percent
                    .map(|value| format!("{value}%"))
                    .unwrap_or_else(|| "—".to_owned());
                let weekly = self
                    .weekly_percent
                    .map(|value| format!("{value}%"))
                    .unwrap_or_else(|| "—".to_owned());
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Usage",
                    format!("5h: {five_hour}\nweek: {weekly}"),
                ));
                Action::None
            }
            "/clear" => Action::ClearScreen,
            "/quit" | "/exit" => Action::Quit,
            unknown => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "알 수 없는 명령",
                    format!("{unknown} — /help로 목록을 확인하세요."),
                ));
                Action::None
            }
        }
    }

    fn handle_pending_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let pending = self.pending.take().expect("pending checked");
        match pending {
            PendingInteraction::ModelPicker {
                mut model_index,
                mut effort_index,
            } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up => {
                        model_index = model_index.saturating_sub(1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('k') if !ctrl && !alt => {
                        model_index = model_index.saturating_sub(1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('p') if ctrl => {
                        model_index = model_index.saturating_sub(1);
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Down => {
                        model_index = (model_index + 1).min(self.models.len().saturating_sub(1));
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('j') if !ctrl && !alt => {
                        model_index = (model_index + 1).min(self.models.len().saturating_sub(1));
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char('n') if ctrl => {
                        model_index = (model_index + 1).min(self.models.len().saturating_sub(1));
                        effort_index = self.effort_index_for_model(model_index);
                    }
                    KeyCode::Char(ch) if !ctrl && !alt && ('1'..='9').contains(&ch) => {
                        let index = ch.to_digit(10).unwrap_or_default() as usize - 1;
                        if index < self.models.len() {
                            let effort_index = self.effort_index_for_model(index);
                            let effort = self
                                .models
                                .get(index)
                                .and_then(|model| model.efforts.get(effort_index))
                                .map(|effort| effort.id.clone());
                            self.apply_model(index, effort.as_deref());
                            return Action::None;
                        }
                    }
                    KeyCode::Left => {
                        effort_index = effort_index.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        let count = self
                            .models
                            .get(model_index)
                            .map(|model| model.efforts.len())
                            .unwrap_or(1)
                            .max(1);
                        effort_index = (effort_index + 1).min(count - 1);
                    }
                    KeyCode::Enter => {
                        let effort = self
                            .models
                            .get(model_index)
                            .and_then(|model| model.efforts.get(effort_index))
                            .map(|effort| effort.id.clone());
                        self.apply_model(model_index, effort.as_deref());
                        return Action::None;
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::ModelPicker {
                    model_index,
                    effort_index,
                });
                Action::None
            }
            PendingInteraction::EffortPicker { mut effort_index } => {
                let count = self
                    .selected_model()
                    .map(|model| model.efforts.len())
                    .unwrap_or(1);
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Left | KeyCode::Up => {
                        effort_index = effort_index.saturating_sub(1);
                    }
                    KeyCode::Char('p') if ctrl => {
                        effort_index = effort_index.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        effort_index = (effort_index + 1).min(count - 1);
                    }
                    KeyCode::Char('n') if ctrl => {
                        effort_index = (effort_index + 1).min(count - 1);
                    }
                    KeyCode::Enter => {
                        let effort = self
                            .selected_model()
                            .and_then(|model| model.efforts.get(effort_index))
                            .map(|effort| effort.id.clone());
                        if let Some(effort) = effort {
                            self.apply_effort(&effort);
                        }
                        return Action::None;
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::EffortPicker { effort_index });
                Action::None
            }
            PendingInteraction::SessionPicker(mut picker) => match picker.handle_key(key) {
                SessionPickerResult::None => {
                    self.pending = Some(PendingInteraction::SessionPicker(picker));
                    Action::None
                }
                SessionPickerResult::Cancel => Action::None,
                SessionPickerResult::Select(thread_id) => Action::ResumeThread(thread_id),
            },
            PendingInteraction::Approval {
                id,
                title,
                detail,
                once,
                session,
                decline,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Action::RpcResponse { id, result: once },
                KeyCode::Char('a') if session.is_some() => Action::RpcResponse {
                    id,
                    result: session.expect("checked"),
                },
                KeyCode::Char('n') | KeyCode::Esc => Action::RpcResponse {
                    id,
                    result: decline,
                },
                _ => {
                    self.pending = Some(PendingInteraction::Approval {
                        id,
                        title,
                        detail,
                        once,
                        session,
                        decline,
                    });
                    Action::None
                }
            },
            PendingInteraction::UserInput {
                id,
                questions,
                current,
                mut selected,
                mut text_mode,
                mut editor,
                mut answers,
            } => {
                if key.code == KeyCode::Esc {
                    return Action::RpcResponse {
                        id,
                        result: answers_response(&answers),
                    };
                }

                let question = &questions[current];
                if text_mode {
                    match key.code {
                        KeyCode::Enter => {
                            let answer = editor.take_for_submit().unwrap_or_default();
                            answers.insert(question.id.clone(), answer);
                            return next_question_or_reply(id, questions, current, answers, self);
                        }
                        KeyCode::Backspace if ctrl => editor.delete_word_left(),
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete => editor.delete(),
                        KeyCode::Left if ctrl || alt => editor.move_word_left(),
                        KeyCode::Right if ctrl || alt => editor.move_word_right(),
                        KeyCode::Left => editor.move_left(),
                        KeyCode::Right => editor.move_right(),
                        KeyCode::Char('w') if ctrl => editor.delete_word_left(),
                        KeyCode::Char('k') if ctrl => editor.delete_to_line_end(),
                        KeyCode::Char('u') if ctrl => editor.delete_to_line_start(),
                        KeyCode::Char('y') if ctrl => editor.yank(),
                        KeyCode::Home => editor.move_home(),
                        KeyCode::End => editor.move_end(),
                        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            editor.insert(ch);
                        }
                        _ => {}
                    }
                } else {
                    let option_count = question.options.len() + usize::from(question.allow_other);
                    match key.code {
                        KeyCode::Up => selected = selected.saturating_sub(1),
                        KeyCode::Down => {
                            selected = (selected + 1).min(option_count.saturating_sub(1))
                        }
                        KeyCode::Enter => {
                            if selected < question.options.len() {
                                answers.insert(
                                    question.id.clone(),
                                    question.options[selected].label.clone(),
                                );
                                return next_question_or_reply(
                                    id, questions, current, answers, self,
                                );
                            }
                            text_mode = true;
                        }
                        _ => {}
                    }
                }
                self.pending = Some(PendingInteraction::UserInput {
                    id,
                    questions,
                    current,
                    selected,
                    text_mode,
                    editor,
                    answers,
                });
                Action::None
            }
        }
    }

    fn overlay_view(&self) -> Option<OverlayView<'_>> {
        match self.pending.as_ref()? {
            PendingInteraction::ModelPicker {
                model_index,
                effort_index,
            } => {
                let start = model_index.saturating_sub(4);
                let end = (start + 9).min(self.models.len());
                let mut lines = self.models[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, model)| {
                        let index = start + offset;
                        OverlayLine {
                            text: format!("{}. {}", index + 1, model.display_name),
                            selected: index == *model_index,
                            muted: false,
                        }
                    })
                    .collect::<Vec<_>>();
                if let Some(model) = self.models.get(*model_index) {
                    lines.push(OverlayLine {
                        text: String::new(),
                        selected: false,
                        muted: true,
                    });
                    lines.push(OverlayLine {
                        text: "Effort".to_owned(),
                        selected: false,
                        muted: false,
                    });
                    lines.extend(
                        effort_slider_rows(model, *effort_index)
                            .into_iter()
                            .enumerate()
                            .map(|(index, text)| OverlayLine {
                                text,
                                selected: false,
                                muted: index != 1,
                            }),
                    );
                }
                Some(OverlayView {
                    title: "Select model".to_owned(),
                    lines,
                    hint: "1-9 select   ↑↓ model   ←→ effort   Enter select   Esc cancel"
                        .to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::EffortPicker { effort_index } => {
                let model = self.selected_model()?;
                let mut lines = vec![
                    OverlayLine {
                        text: model.display_name.clone(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: String::new(),
                        selected: false,
                        muted: true,
                    },
                ];
                lines.extend(
                    effort_slider_rows(model, *effort_index)
                        .into_iter()
                        .enumerate()
                        .map(|(index, text)| OverlayLine {
                            text,
                            selected: false,
                            muted: index != 1,
                        }),
                );
                Some(OverlayView {
                    title: "Set effort".to_owned(),
                    lines,
                    hint: "←→ adjust   Enter apply   Esc cancel".to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::SessionPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::Approval {
                title,
                detail,
                session,
                ..
            } => {
                let mut lines = detail
                    .iter()
                    .map(|text| OverlayLine {
                        text: text.clone(),
                        selected: false,
                        muted: false,
                    })
                    .collect::<Vec<_>>();
                lines.push(OverlayLine {
                    text: "[y] 이번만 허용".to_owned(),
                    selected: true,
                    muted: false,
                });
                if session.is_some() {
                    lines.push(OverlayLine {
                        text: "[a] 세션 동안 허용".to_owned(),
                        selected: false,
                        muted: false,
                    });
                }
                lines.push(OverlayLine {
                    text: "[n] 거부".to_owned(),
                    selected: false,
                    muted: false,
                });
                Some(OverlayView {
                    title: title.clone(),
                    lines,
                    hint: "y / a / n".to_owned(),
                    style: OverlayStyle::Panel,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::UserInput {
                questions,
                current,
                selected,
                text_mode,
                editor,
                ..
            } => {
                let question = &questions[*current];
                let mut lines = vec![OverlayLine {
                    text: question.question.clone(),
                    selected: false,
                    muted: false,
                }];
                if !text_mode {
                    lines.extend(question.options.iter().enumerate().map(|(index, option)| {
                        OverlayLine {
                            text: format!("{}\n      {}", option.label, option.description),
                            selected: index == *selected,
                            muted: false,
                        }
                    }));
                    if question.allow_other {
                        lines.push(OverlayLine {
                            text: "직접 입력".to_owned(),
                            selected: *selected == question.options.len(),
                            muted: false,
                        });
                    }
                }
                Some(OverlayView {
                    title: if question.header.is_empty() {
                        format!("Question {}/{}", current + 1, questions.len())
                    } else {
                        question.header.clone()
                    },
                    lines,
                    hint: if *text_mode {
                        "답을 입력하고 Enter · Esc 취소".to_owned()
                    } else {
                        "↑↓ 선택  Enter 확인  Esc 취소".to_owned()
                    },
                    style: OverlayStyle::Panel,
                    input: text_mode.then_some(editor),
                    input_label: "Answer",
                    input_placeholder: "Type your answer…",
                })
            }
        }
    }

    fn matching_slash_commands(&self) -> Vec<&'static SlashCommand> {
        let text = self.editor.text();
        if !text.starts_with('/') || text.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .filter(|command| command.name.starts_with(&text))
            .collect()
    }

    fn slash_suggestion_views(&self) -> Vec<SuggestionView> {
        self.matching_slash_commands()
            .into_iter()
            .enumerate()
            .map(|(index, command)| SuggestionView {
                command: command.name.to_owned(),
                description: command.description.to_owned(),
                selected: index == self.command_selection,
            })
            .collect()
    }

    fn activity(&self) -> Option<String> {
        if !self.busy {
            return None;
        }
        let elapsed = self
            .turn_started_at
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0);
        Some(format!(
            "{} Working… {}s · Esc to interrupt",
            SPINNER[self.spinner_frame], elapsed
        ))
    }

    fn status_line(&self) -> StatusLineView {
        let context = self.context_window.and_then(|window| {
            (window > 0).then(|| {
                format!(
                    "ctx: {}/{} ({}%)",
                    format_token_count(self.total_tokens),
                    format_token_count(window),
                    self.total_tokens.saturating_mul(100) / window
                )
            })
        });
        StatusLineView {
            branch: self.branch.clone(),
            model: self.selected_model_display_name().to_owned(),
            effort: self.selected_effort.clone(),
            context,
            five_hour_percent: self.five_hour_percent,
            weekly_percent: self.weekly_percent,
            fast_mode: self.effective_fast_mode(),
            notice: self.transient_status.clone(),
        }
    }

    fn apply_model(&mut self, index: usize, effort: Option<&str>) {
        self.commit_welcome_card();
        let Some(model) = self.models.get(index) else {
            return;
        };
        let selected_effort = effort
            .filter(|effort| model.supports_effort(effort))
            .unwrap_or(&model.default_effort)
            .to_owned();
        let model_name = model.display_name.clone();
        let context_window = model.context_window;
        self.selected_model = index;
        self.selected_effort = selected_effort.clone();
        self.context_window = context_window.or(self.context_window);
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            "✓ Model changed",
            format!("↳ {model_name} · {selected_effort}"),
        ));
    }

    fn cycle_permission_mode(&mut self) {
        self.permission_mode = self.permission_mode.next();
    }

    fn apply_effort(&mut self, effort: &str) {
        self.commit_welcome_card();
        let Some(model) = self.selected_model() else {
            return;
        };
        if !model.supports_effort(effort) {
            return;
        }
        let model_name = model.display_name.clone();
        self.selected_effort = effort.to_owned();
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            "✓ Effort changed",
            format!("↳ {model_name} · {effort}"),
        ));
    }

    fn commit_welcome_card(&mut self) {
        if !self.show_welcome {
            return;
        }
        self.committed.push(Block::welcome(
            self.selected_model_display_name(),
            &self.selected_effort,
            &self.cwd,
            &self.account,
        ));
        self.show_welcome = false;
    }

    fn effort_index_for_model(&self, model_index: usize) -> usize {
        let Some(model) = self.models.get(model_index) else {
            return 0;
        };
        model
            .efforts
            .iter()
            .position(|effort| effort.id == self.selected_effort)
            .or_else(|| {
                model
                    .efforts
                    .iter()
                    .position(|effort| effort.id == model.default_effort)
            })
            .unwrap_or(0)
    }

    fn start_item(&mut self, item: &Value) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(block) = active_item_block(item) else {
            return;
        };
        if !self.active.contains_key(id) {
            self.active_order.push(id.to_owned());
        }
        self.active.insert(id.to_owned(), ActiveItem { block });
    }

    fn complete_item(&mut self, item: &Value) {
        let id = item.get("id").and_then(Value::as_str);
        if let Some(id) = id {
            self.active.remove(id);
            self.active_order.retain(|candidate| candidate != id);
        }
        if item.get("type").and_then(Value::as_str) == Some("userMessage") {
            return;
        }
        if let Some(block) = completed_item_block(item) {
            if matches!(block.kind, BlockKind::Assistant) {
                self.last_assistant_markdown = Some(block.body.clone());
            }
            self.committed.push(block);
        }
    }

    fn append_delta(&mut self, params: &Value, kind: BlockKind, title: &str) {
        let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
            return;
        };
        let Some(delta) = params.get("delta").and_then(Value::as_str) else {
            return;
        };
        append_capped(
            &mut self.ensure_active(item_id, kind, title).block.body,
            delta,
        );
    }

    fn ensure_active(&mut self, item_id: &str, kind: BlockKind, title: &str) -> &mut ActiveItem {
        if !self.active.contains_key(item_id) {
            self.active_order.push(item_id.to_owned());
            self.active.insert(
                item_id.to_owned(),
                ActiveItem {
                    block: Block::new(kind, title, ""),
                },
            );
        }
        self.active.get_mut(item_id).expect("inserted")
    }

    fn flush_orphaned_active(&mut self) {
        for id in std::mem::take(&mut self.active_order) {
            if let Some(item) = self.active.remove(&id) {
                if matches!(item.block.kind, BlockKind::Assistant) {
                    self.last_assistant_markdown = Some(item.block.body.clone());
                }
                self.committed.push(item.block);
            }
        }
    }
}

fn next_question_or_reply(
    id: Value,
    questions: Vec<Question>,
    current: usize,
    answers: BTreeMap<String, String>,
    state: &mut AppState,
) -> Action {
    if current + 1 == questions.len() {
        return Action::RpcResponse {
            id,
            result: answers_response(&answers),
        };
    }
    let next = current + 1;
    let text_mode = questions[next].options.is_empty();
    state.pending = Some(PendingInteraction::UserInput {
        id,
        questions,
        current: next,
        selected: 0,
        text_mode,
        editor: Editor::default(),
        answers,
    });
    Action::None
}

fn answers_response(answers: &BTreeMap<String, String>) -> Value {
    let mut map = Map::new();
    for (id, answer) in answers {
        map.insert(id.clone(), json!({ "answers": [answer] }));
    }
    json!({ "answers": map })
}

fn parse_questions(params: &Value) -> Vec<Question> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    Some(QuestionOption {
                        label: option.get("label")?.as_str()?.to_owned(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect();
            Some(Question {
                id: question.get("id")?.as_str()?.to_owned(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                question: question.get("question")?.as_str()?.to_owned(),
                options,
                allow_other: question
                    .get("isOther")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect()
}

fn active_item_block(item: &Value) -> Option<Block> {
    match item.get("type")?.as_str()? {
        "agentMessage" => Some(Block::new(
            BlockKind::Assistant,
            "Codex",
            item.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "reasoning" => Some(Block::new(
            BlockKind::Reasoning,
            "Thinking…",
            string_array(item.get("summary")),
        )),
        "plan" => Some(Block::new(
            BlockKind::Reasoning,
            "Plan",
            item.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "commandExecution" => Some(Block::new(
            BlockKind::Tool,
            format!(
                "Bash · {}",
                compact_command(
                    item.get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("command"),
                    88
                )
            ),
            item.get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
        "fileChange" => Some(Block::new(
            BlockKind::Tool,
            "Update files",
            file_changes_body(
                item.get("changes")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            ),
        )),
        "mcpToolCall" => Some(Block::new(
            BlockKind::Tool,
            format!(
                "MCP · {} › {}",
                item.get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("server"),
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
            pretty_json(item.get("arguments")),
        )),
        "dynamicToolCall" => Some(Block::new(
            BlockKind::Tool,
            format!(
                "Tool · {}",
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
            pretty_json(item.get("arguments")),
        )),
        "webSearch" => Some(Block::new(BlockKind::Tool, "Web search", "")),
        "collabAgentToolCall" => Some(Block::new(
            BlockKind::Tool,
            "Agent",
            item.get("tool").map(Value::to_string).unwrap_or_default(),
        )),
        _ => None,
    }
}

fn completed_item_block(item: &Value) -> Option<Block> {
    match item.get("type")?.as_str()? {
        "userMessage" => {
            let body = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|content| {
                    (content.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| content.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!body.is_empty()).then(|| Block::new(BlockKind::User, "You", body))
        }
        "commandExecution" => {
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let exit = item.get("exitCode").and_then(Value::as_i64);
            let suffix = exit
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let duration = item
                .get("durationMs")
                .and_then(Value::as_u64)
                .map(format_duration)
                .map(|duration| format!(" · {duration}"))
                .unwrap_or_default();
            Some(Block::new(
                if status == "completed" {
                    BlockKind::Tool
                } else {
                    BlockKind::Warning
                },
                format!(
                    "Bash · {}{suffix}{duration}",
                    compact_command(
                        item.get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("command"),
                        88
                    )
                ),
                collapse_output(
                    item.get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    14,
                ),
            ))
        }
        "fileChange" => Some(Block::new(
            BlockKind::Tool,
            "Updated files",
            file_changes_body(
                item.get("changes")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            ),
        )),
        "mcpToolCall" => {
            let mut block = active_item_block(item)?;
            block.body = match item.get("error").filter(|value| !value.is_null()) {
                Some(error) => pretty_json(Some(error)),
                None => pretty_json(item.get("result")),
            };
            Some(block)
        }
        "dynamicToolCall" => {
            let mut block = active_item_block(item)?;
            block.body = pretty_json(item.get("contentItems"));
            Some(block)
        }
        "contextCompaction" => Some(Block::new(BlockKind::System, "Context compacted", "")),
        _ => active_item_block(item),
    }
}

fn permission_detail(value: &Value) -> Vec<String> {
    let mut detail = Vec::new();
    if let Some(enabled) = value
        .get("network")
        .and_then(|network| network.get("enabled"))
        .and_then(Value::as_bool)
    {
        detail.push(format!(
            "네트워크: {}",
            if enabled { "허용" } else { "차단" }
        ));
    }
    if let Some(file_system) = value.get("fileSystem").filter(|value| !value.is_null()) {
        detail.push(format!("파일 시스템: {}", pretty_json(Some(file_system))));
    }
    if detail.is_empty() {
        detail.push(pretty_json(Some(value)));
    }
    detail
}

fn file_changes_body(changes: &[Value]) -> String {
    changes
        .iter()
        .map(|change| {
            let path = change
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let kind = change
                .get("kind")
                .and_then(|kind| kind.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("update");
            let diff = change
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let additions = diff
                .lines()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                .count();
            let deletions = diff
                .lines()
                .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                .count();
            let stats = match (additions, deletions) {
                (0, 0) => String::new(),
                _ => format!("  +{additions} -{deletions}"),
            };
            let marker = match kind {
                "add" => "+",
                "delete" => "-",
                _ => "±",
            };
            format!("{marker}  {path}{stats}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms}ms")
    } else {
        format!("{:.1}s", duration_ms as f64 / 1_000.0)
    }
}

fn effort_slider_rows(model: &ModelInfo, selected: usize) -> [String; 3] {
    const SLOT_WIDTH: usize = 8;
    let count = model.efforts.len().max(1);
    let width = count * SLOT_WIDTH;
    let endpoints = format!(
        "Faster{}Smarter",
        " ".repeat(width.saturating_sub("Faster".len() + "Smarter".len()))
    );
    let track = model
        .efforts
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index == selected {
                "───●────"
            } else {
                "───○────"
            }
        })
        .collect::<String>();
    let labels = model
        .efforts
        .iter()
        .map(|effort| center_cell(&effort.id, SLOT_WIDTH))
        .collect::<String>()
        .trim_end()
        .to_owned();
    [endpoints, track, labels]
}

fn center_cell(text: &str, width: usize) -> String {
    let text_width = text.chars().count().min(width);
    let left = width.saturating_sub(text_width) / 2;
    let right = width.saturating_sub(text_width + left);
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

fn relative_time(timestamp: u64) -> String {
    if timestamp == 0 {
        return "unknown".to_owned();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(timestamp);
    let elapsed = now.saturating_sub(timestamp);
    match elapsed {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        86_400..=604_799 => format!("{}d ago", elapsed / 86_400),
        _ => format!("{}w ago", elapsed / 604_800),
    }
}

fn path_eq(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn append_capped(target: &mut String, delta: &str) {
    const MAX_ACTIVE_BYTES: usize = 128 * 1024;
    target.push_str(delta);
    if target.len() > MAX_ACTIVE_BYTES {
        let keep_from = target.len() - MAX_ACTIVE_BYTES;
        let boundary = target
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= keep_from)
            .unwrap_or(keep_from);
        target.replace_range(..boundary, "…\n");
    }
}

fn collapse_output(output: &str, max_lines: usize) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() <= max_lines {
        return output.trim_end().to_owned();
    }
    let head = max_lines / 2;
    let tail = max_lines - head;
    format!(
        "{}\n… {} lines hidden …\n{}",
        lines[..head].join("\n"),
        lines.len() - max_lines,
        lines[lines.len() - tail..].join("\n")
    )
}

fn string_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn pretty_json(value: Option<&Value>) -> String {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return String::new();
    };
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn compact_command(command: &str, max_chars: usize) -> String {
    let one_line = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    format!(
        "{}…",
        one_line
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}

fn format_token_count(tokens: u64) -> String {
    format!("{}k", tokens.saturating_add(500) / 1_000)
}

fn read_git_branch(cwd: &str) -> Option<String> {
    let mut directory = PathBuf::from(cwd);
    for _ in 0..10 {
        let marker = directory.join(".git");
        let head = if marker.is_dir() {
            fs::read_to_string(marker.join("HEAD")).ok()
        } else if marker.is_file() {
            fs::read_to_string(&marker).ok().and_then(|git_file| {
                let git_dir = git_file.trim().strip_prefix("gitdir:")?.trim();
                let git_dir = Path::new(git_dir);
                let git_dir = if git_dir.is_absolute() {
                    git_dir.to_owned()
                } else {
                    directory.join(git_dir)
                };
                fs::read_to_string(git_dir.join("HEAD")).ok()
            })
        } else {
            None
        };
        if let Some(branch) = head.as_deref().and_then(parse_git_branch) {
            return Some(branch);
        }
        if !directory.pop() {
            break;
        }
    }
    None
}

fn parse_git_branch(head: &str) -> Option<String> {
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(ToOwned::to_owned)
        .or_else(|| (head.chars().count() >= 7).then(|| head.chars().take(7).collect::<String>()))
}

fn read_codex_usage() -> (Option<u8>, Option<u8>) {
    let Some(path) = env::var_os("APPDATA").map(|app_data| {
        PathBuf::from(app_data)
            .join("DevezCode")
            .join("codex-usage.json")
    }) else {
        return (None, None);
    };
    let Some(root) = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return (None, None);
    };
    parse_codex_usage(&root)
}

pub fn load_model_context_windows(models: &mut [ModelInfo]) {
    let Some(root) = codex_home()
        .and_then(|home| fs::read_to_string(home.join("models_cache.json")).ok())
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return;
    };
    apply_model_context_cache(models, &root);
}

fn apply_model_context_cache(models: &mut [ModelInfo], root: &Value) {
    let Some(cached_models) = root.get("models").and_then(Value::as_array) else {
        return;
    };
    for model in models
        .iter_mut()
        .filter(|model| model.context_window.is_none())
    {
        let Some(cached) = cached_models.iter().find(|cached| {
            cached
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| slug == model.id || slug == model.model)
        }) else {
            continue;
        };
        let Some(raw_window) = cached.get("context_window").and_then(Value::as_u64) else {
            continue;
        };
        let effective_percent = cached
            .get("effective_context_window_percent")
            .and_then(Value::as_u64)
            .unwrap_or(100);
        model.context_window = Some(raw_window.saturating_mul(effective_percent) / 100);
    }
}

fn parse_codex_usage(root: &Value) -> (Option<u8>, Option<u8>) {
    (
        usage_percent(root, "five_hour"),
        usage_percent(root, "weekly"),
    )
}

fn usage_percent(root: &Value, key: &str) -> Option<u8> {
    root.get(key)?
        .get("used_percent")?
        .as_f64()
        .map(|percent| percent.round().clamp(0.0, 100.0) as u8)
}

fn read_fast_mode() -> bool {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .is_some_and(|config| parse_fast_mode(&config))
}

fn read_permission_mode() -> PermissionMode {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .and_then(|config| parse_permission_mode(&config))
        .unwrap_or(PermissionMode::Default)
}

fn parse_permission_mode(config: &str) -> Option<PermissionMode> {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "sandbox_mode").then(|| {
                match value
                    .trim()
                    .trim_matches(['"', '\''])
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "read-only" => Some(PermissionMode::ReadOnly),
                    "workspace-write" => Some(PermissionMode::Default),
                    "danger-full-access" => Some(PermissionMode::FullAccess),
                    _ => None,
                }
            })
        })
        .flatten()
}

fn codex_home() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex")))
}

fn parse_fast_mode(config: &str) -> bool {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "service_tier").then(|| {
                matches!(
                    value
                        .trim()
                        .trim_matches(['"', '\''])
                        .to_ascii_lowercase()
                        .as_str(),
                    "fast" | "priority"
                )
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model(slug: &str, display_name: &str, is_default: bool) -> ModelInfo {
        ModelInfo {
            id: slug.to_owned(),
            model: slug.to_owned(),
            display_name: display_name.to_owned(),
            efforts: ["low", "medium", "high", "xhigh", "max", "ultra"]
                .into_iter()
                .map(|id| EffortInfo { id: id.to_owned() })
                .collect(),
            default_effort: "high".to_owned(),
            is_default,
            context_window: None,
            fast_service_tier: Some("priority".to_owned()),
        }
    }

    fn test_state() -> AppState {
        AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![test_model("gpt-5.6-sol", "GPT-5.6 Sol", true)],
            "gpt-5.6-sol",
            Some("high"),
        )
    }

    #[test]
    fn shift_tab_cycles_permission_modes_and_wraps() {
        let mut state = test_state();
        state.permission_mode = PermissionMode::ReadOnly;

        let expected = [
            PermissionMode::Default,
            PermissionMode::FullAccess,
            PermissionMode::ReadOnly,
        ];
        for mode in expected {
            let action = state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

            assert!(matches!(action, Action::None));
            assert_eq!(state.permission_mode(), mode);
        }
        assert_eq!(state.permission_profile(), ":read-only");
    }

    #[test]
    fn shift_tab_still_cycles_while_a_slash_command_is_being_typed() {
        let mut state = test_state();
        state.permission_mode = PermissionMode::Default;
        state.editor.insert_str("/mo");

        state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(state.permission_mode(), PermissionMode::FullAccess);
        assert_eq!(state.editor.text(), "/mo");
    }

    #[test]
    fn permission_mode_starts_from_the_configured_sandbox_mode() {
        assert_eq!(
            parse_permission_mode("sandbox_mode = \"danger-full-access\"\n"),
            Some(PermissionMode::FullAccess)
        );
        assert_eq!(
            parse_permission_mode("approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n"),
            Some(PermissionMode::ReadOnly)
        );
        assert_eq!(
            parse_permission_mode("sandbox_mode = \"workspace-write\"\n"),
            Some(PermissionMode::Default)
        );
        // Values under a table header belong to that table, not the root config.
        assert_eq!(
            parse_permission_mode("[windows]\nsandbox_mode = \"read-only\"\n"),
            None
        );
        assert_eq!(parse_permission_mode("model = \"gpt-5.6\"\n"), None);
    }

    #[test]
    fn status_command_reports_the_active_permission_profile() {
        let mut state = test_state();
        state.permission_mode = PermissionMode::FullAccess;

        state.run_slash_command("/status");

        let body = &state.committed.last().expect("status block").body;
        assert!(body.contains("permissions: Full Access (:danger-full-access)"));
    }

    #[test]
    fn modified_enter_keys_insert_newlines_without_submitting() {
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SHIFT] {
            let mut state = test_state();
            state.editor.insert_str("first");

            let action = state.handle_key(KeyEvent::new(KeyCode::Enter, modifiers));

            assert!(matches!(action, Action::None));
            assert_eq!(state.editor.text(), "first\n");
            assert!(!state.busy);
        }
    }

    #[test]
    fn fast_and_btw_commands_dispatch_real_actions() {
        let mut state = test_state();
        assert!(matches!(
            state.run_slash_command("/fast on"),
            Action::SetFast(true)
        ));
        assert!(matches!(
            state.run_slash_command("/btw quick question"),
            Action::StartSide(Some(message)) if message == "quick question"
        ));
    }

    #[test]
    fn fast_mode_is_shown_only_by_the_persistent_statusline() {
        let mut state = test_state();

        state.set_fast_mode(true);

        assert!(state.fast_mode);
        assert!(state.committed.is_empty());
        assert!(state.transient_status.is_none());
    }

    #[test]
    fn pending_output_is_always_placed_below_the_welcome_card() {
        let mut state = test_state();
        state.push_notice(BlockKind::System, "Done", "Ready");

        let blocks = state.drain_committed();

        assert!(matches!(blocks[0].kind, BlockKind::Welcome));
        assert!(matches!(blocks[1].kind, BlockKind::System));
        assert!(!state.show_welcome);
    }

    #[test]
    fn model_change_commits_welcome_before_the_change_card() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6 Terra", false));

        state.apply_model(1, Some("xhigh"));

        assert!(matches!(state.committed[0].kind, BlockKind::Welcome));
        assert!(matches!(state.committed[1].kind, BlockKind::ModelChange));
        assert_eq!(state.committed[1].title, "✓ Model changed");
        assert!(state.committed[1].body.starts_with("↳ "));
        assert!(!state.show_welcome);
    }

    #[test]
    fn first_prompt_keeps_the_welcome_card_above_the_user_message() {
        let mut state = test_state();
        state.editor.insert_str("hello");

        let action = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(action, Action::Submit(message) if message == "hello"));
        assert!(matches!(state.committed[0].kind, BlockKind::Welcome));
        assert!(matches!(state.committed[1].kind, BlockKind::User));
        assert!(!state.show_welcome);
    }

    #[test]
    fn effort_change_uses_the_same_checked_card_as_model_change() {
        let mut state = test_state();

        state.apply_effort("xhigh");

        let card = state.committed.last().expect("effort card");
        assert!(matches!(card.kind, BlockKind::ModelChange));
        assert_eq!(card.title, "✓ Effort changed");
        assert_eq!(card.body, "↳ GPT-5.6 Sol · xhigh");
    }

    #[test]
    fn model_catalog_exposes_the_fast_service_tier() {
        let model = ModelInfo::from_value(&json!({
            "id": "gpt-5.6-sol",
            "model": "gpt-5.6-sol",
            "displayName": "GPT-5.6 Sol",
            "supportedReasoningEfforts": [{"reasoningEffort": "high"}],
            "defaultReasoningEffort": "high",
            "serviceTiers": [{
                "id": "priority",
                "name": "Fast",
                "description": "1.5x speed"
            }]
        }))
        .expect("model");

        assert_eq!(model.fast_service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn session_picker_scopes_to_cwd_and_can_expand_to_all_projects() {
        let sessions = vec![
            SessionInfo {
                id: "current".to_owned(),
                name: Some("Current project".to_owned()),
                preview: String::new(),
                cwd: r"C:\work\current".to_owned(),
                updated_at: 2,
            },
            SessionInfo {
                id: "other".to_owned(),
                name: Some("Other project".to_owned()),
                preview: String::new(),
                cwd: r"C:\work\other".to_owned(),
                updated_at: 1,
            },
        ];
        let mut picker = SessionPicker::new(sessions, r"C:\work\current".to_owned(), None);

        assert_eq!(picker.filtered_len(), 1);
        picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(picker.filtered_len(), 2);
    }

    #[test]
    fn effort_requires_an_explicit_supported_level() {
        let model = ModelInfo {
            id: "model".to_owned(),
            model: "model".to_owned(),
            display_name: "Model".to_owned(),
            efforts: vec![
                EffortInfo {
                    id: "high".to_owned(),
                },
                EffortInfo {
                    id: "max".to_owned(),
                },
            ],
            default_effort: "high".to_owned(),
            is_default: true,
            context_window: None,
            fast_service_tier: Some("priority".to_owned()),
        };
        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "model",
            Some("high"),
        );

        state.run_slash_command("/effort auto");
        assert_eq!(state.selected_effort(), "high");

        state.run_slash_command("/effort max");
        assert_eq!(state.selected_effort(), "max");
    }

    #[test]
    fn model_aliases_and_number_keys_select_catalog_entries() {
        let models = vec![
            test_model("gpt-5.6-sol", "GPT-5.6-Sol", true),
            test_model("gpt-5.6-terra", "GPT-5.6-Terra", false),
            test_model("gpt-5.6-luna", "GPT-5.6-Luna", false),
            test_model("gpt-5.5", "GPT-5.5", false),
        ];
        assert!(models[0].matches_query("sol"));
        assert!(models[1].matches_query("terra"));
        assert!(models[2].matches_query("luna"));
        assert!(models[3].matches_query("5.5"));

        let mut state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "gpt-5.6-sol",
            Some("high"),
        );
        state.run_slash_command("/model terra");
        assert_eq!(state.selected_model_display_name(), "GPT-5.6-Terra");

        state.run_slash_command("/model");
        let overlay = state.overlay_view().expect("model picker");
        assert!(overlay.lines[0].text.starts_with("1. "));
        assert!(overlay.lines[1].text.starts_with("2. "));
        state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(state.selected_model_display_name(), "GPT-5.6-Sol");
    }

    #[test]
    fn effort_slider_has_direction_track_and_aligned_labels() {
        let model = test_model("gpt-5.6-sol", "GPT-5.6-Sol", true);
        let [endpoints, track, labels] = effort_slider_rows(&model, 2);

        assert!(endpoints.starts_with("Faster"));
        assert!(endpoints.ends_with("Smarter"));
        assert_eq!(
            track.chars().filter(|ch| matches!(ch, '○' | '●')).count(),
            6
        );
        assert_eq!(track.chars().filter(|ch| *ch == '●').count(), 1);
        assert!(labels.contains("low"));
        assert!(labels.contains("ultra"));
        assert_eq!(endpoints.chars().count(), track.chars().count());
    }

    #[test]
    fn new_thread_resets_the_conversation_view() {
        let model = test_model("gpt-5.6-sol", "GPT-5.6-Sol", true);
        let mut state = AppState::new(
            "old-thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "gpt-5.6-sol",
            Some("high"),
        );
        state.editor.set_text("leftover input");
        state
            .committed
            .push(Block::new(BlockKind::Assistant, "", "old response"));
        state.total_tokens = 42;
        state.context_window = Some(100);
        state.transient_status = Some("old status".to_owned());
        state.busy = true;
        state.turn_id = Some("old-turn".to_owned());
        state.turn_started_at = Some(Instant::now());

        state.prepare_new_thread();

        assert!(state.editor.is_empty());
        assert!(state.committed.is_empty());
        assert_eq!(state.total_tokens, 0);
        assert_eq!(state.context_window, None);
        assert_eq!(state.transient_status, None);
        assert!(!state.busy);
        assert_eq!(state.turn_id, None);
        assert!(state.turn_started_at.is_none());
        assert!(state.view().welcome.is_some());
    }

    #[test]
    fn status_metadata_parses_usage_fast_mode_and_branch() {
        let usage = json!({
            "five_hour": { "used_percent": 12.4 },
            "weekly": { "used_percent": 70 }
        });

        assert_eq!(parse_codex_usage(&usage), (Some(12), Some(70)));
        assert!(parse_fast_mode(
            "service_tier = \"fast\"\n[features]\nexample = true"
        ));
        assert!(!parse_fast_mode("service_tier = \"default\""));
        assert_eq!(
            parse_git_branch("ref: refs/heads/feature/status-line\n"),
            Some("feature/status-line".to_owned())
        );
    }

    #[test]
    fn model_cache_seeds_effective_context_before_the_first_turn() {
        let mut models = vec![test_model("gpt-5.6-sol", "GPT-5.6-Sol", true)];
        apply_model_context_cache(
            &mut models,
            &json!({
                "models": [{
                    "slug": "gpt-5.6-sol",
                    "context_window": 272_000,
                    "effective_context_window_percent": 95
                }]
            }),
        );
        assert_eq!(models[0].context_window, Some(258_400));

        let state = AppState::new(
            "thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            models,
            "gpt-5.6-sol",
            Some("high"),
        );
        assert_eq!(
            state.view().status_line.and_then(|status| status.context),
            Some("ctx: 0k/258k (0%)".to_owned())
        );
    }
}
