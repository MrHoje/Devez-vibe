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
    pricing::{self, TokenTotals},
    renderer::{
        Block, BlockKind, ComposerMode, ModeAccent, OverlayLine, OverlayStyle, OverlayView,
        StatusLineView, SuggestionView, View, WelcomeView,
    },
    theme::{self, ThemeKind},
};

pub const SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

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

const SLASH_COMMANDS: [SlashCommand; 24] = [
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
        name: "/theme",
        description: "Switch Minimal, Soft, or Dark theme",
        takes_argument: true,
    },
    SlashCommand {
        name: "/login",
        description: "Sign in to a ChatGPT account",
        takes_argument: false,
    },
    SlashCommand {
        name: "/logout",
        description: "Sign out of the current account",
        takes_argument: false,
    },
    SlashCommand {
        name: "/mcp",
        description: "Show MCP servers or start OAuth login",
        takes_argument: true,
    },
    SlashCommand {
        name: "/plugins",
        description: "List, install, enable, disable, or uninstall plugins",
        takes_argument: true,
    },
    SlashCommand {
        name: "/skills",
        description: "List, enable, or disable Codex skills",
        takes_argument: true,
    },
    SlashCommand {
        name: "/apps",
        description: "List, enable, or disable Codex apps",
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

/// At most this many credits are listed before the rest are summarised.
const CREDIT_LIST_LIMIT: usize = 4;

/// One `available` rate-limit reset credit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResetCredit {
    /// Server-supplied label, for example `Full reset`.
    pub title: String,
    /// Expiry as a Unix timestamp; `None` when the server reported no expiry.
    pub expires_at: Option<u64>,
}

/// Plan and rate-limit-reset entitlements, from `account/rateLimits/read`.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct AccountPlan {
    /// Display label such as `Pro Lite`; `None` when the server did not report one.
    pub plan: Option<String>,
    /// Reset credits still marked `available`, soonest expiry first.
    pub credits: Vec<ResetCredit>,
    /// The server's own tally, which can exceed the listed credits.
    pub available_credits: usize,
}

impl AccountPlan {
    pub fn from_rate_limits(root: &Value) -> Self {
        let plan = root
            .get("rateLimits")
            .and_then(|limits| limits.get("planType"))
            .and_then(Value::as_str)
            .filter(|plan| !plan.is_empty())
            .map(plan_label);

        let reset_credits = root.get("rateLimitResetCredits");
        let mut credits = reset_credits
            .and_then(|credits| credits.get("credits"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|credit| {
                credit
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status.eq_ignore_ascii_case("available"))
            })
            .map(|credit| ResetCredit {
                title: credit
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Reset")
                    .to_owned(),
                expires_at: credit.get("expiresAt").and_then(Value::as_u64),
            })
            .collect::<Vec<_>>();
        // Soonest expiry first; undated credits sink to the bottom.
        credits.sort_by_key(|credit| credit.expires_at.unwrap_or(u64::MAX));

        // Trust the server's tally when present; it counts what it will honour.
        let available_credits = reset_credits
            .and_then(|credits| credits.get("availableCount"))
            .and_then(Value::as_u64)
            .map_or(credits.len(), |count| count as usize);

        Self {
            plan,
            credits,
            available_credits,
        }
    }

    pub fn plan_display(&self) -> String {
        self.plan.clone().unwrap_or_else(|| "—".to_owned())
    }

    /// Welcome-panel rows: a summary followed by one line per credit.
    pub fn credit_lines(&self) -> Vec<String> {
        self.credit_lines_at(unix_now())
    }

    /// Split out from [`Self::credit_lines`] so the wording is testable.
    fn credit_lines_at(&self, now: u64) -> Vec<String> {
        if self.available_credits == 0 && self.credits.is_empty() {
            return vec!["none available".to_owned()];
        }
        let mut lines = vec![format!("{} available", self.available_credits)];
        for credit in self.credits.iter().take(CREDIT_LIST_LIMIT) {
            lines.push(format!(
                "· {}",
                match credit.expires_at {
                    Some(expiry) if expiry > now => format!(
                        "{}  {} left",
                        format_date(expiry),
                        short_duration(expiry - now)
                    ),
                    Some(expiry) => format!("{}  expired", format_date(expiry)),
                    None => format!("{}  no expiry", credit.title),
                }
            ));
        }
        if let Some(hidden) = self.credits.len().checked_sub(CREDIT_LIST_LIMIT)
            && hidden > 0
        {
            lines.push(format!("· +{hidden} more"));
        }
        lines
    }
}

/// Maps the server's `planType` onto the wording OpenAI uses for the plan.
fn plan_label(raw: &str) -> String {
    let key = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match key.as_str() {
        "free" => "Free".to_owned(),
        "go" => "Go".to_owned(),
        "plus" => "Plus".to_owned(),
        // The two Pro tiers are named by their usage multiplier rather than by
        // the server's slug, which says nothing about how much quota you get.
        "prolite" => "Pro 5x".to_owned(),
        "pro" => "Pro 20x".to_owned(),
        "team" => "Team".to_owned(),
        "selfservebusinessusagebased" => "Business (usage-based)".to_owned(),
        "business" => "Business".to_owned(),
        "enterprisecbpusagebased" => "Enterprise (usage-based)".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        "edu" => "Edu".to_owned(),
        _ => title_case(raw),
    }
}

fn title_case(raw: &str) -> String {
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Local-date label (`2026-08-01`) for a Unix timestamp.
fn format_date(timestamp: u64) -> String {
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|instant| {
            instant
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Coarse "12d" / "5h" / "8m" label for a future span.
fn short_duration(seconds: u64) -> String {
    match seconds {
        0..=59 => "<1m".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

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
    ShowMcp,
    McpLogin(String),
    StartLogin(LoginMethod),
    CancelLogin(String),
    Logout,
    ShowPlugins,
    PreparePluginInstall(String),
    PreparePluginUninstall(String),
    SetPlugin { query: String, enabled: bool },
    InstallPlugin(PluginInstallTarget),
    UninstallPlugin(PluginUninstallTarget),
    ShowSkills,
    SetSkill { name: String, enabled: bool },
    ShowApps,
    SetApp { query: String, enabled: bool },
    RefreshSkills,
    OpenUrl(String),
    SetTheme(ThemeKind),
    Quit,
    ClearScreen,
    Tick(bool),
    RpcResponse { id: Value, result: Value },
    RpcError { id: Value, message: String },
}

pub struct PluginInstallTarget {
    pub plugin_name: String,
    pub marketplace_path: Option<String>,
    pub remote_marketplace_name: Option<String>,
}

pub struct PluginUninstallTarget {
    pub plugin_id: String,
    pub display_name: String,
}

/// Sign-in flows offered by `/login`, mirroring `LoginAccountParams` variants
/// that do not require pasting a secret into the composer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoginMethod {
    Browser,
    DeviceCode,
}

impl LoginMethod {
    const CHOICES: [Self; 2] = [Self::Browser, Self::DeviceCode];

    /// `LoginAccountParams` tag sent to `account/login/start`.
    pub fn param_type(self) -> &'static str {
        match self {
            Self::Browser => "chatgpt",
            Self::DeviceCode => "chatgptDeviceCode",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Browser => "ChatGPT 계정으로 로그인",
            Self::DeviceCode => "기기 코드로 로그인",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Browser => "브라우저가 열립니다",
            Self::DeviceCode => "코드를 다른 기기에 입력합니다",
        }
    }
}

enum ConfirmedAction {
    InstallPlugin(PluginInstallTarget),
    UninstallPlugin(PluginUninstallTarget),
    Logout,
}

impl ConfirmedAction {
    fn into_action(self) -> Action {
        match self {
            Self::InstallPlugin(target) => Action::InstallPlugin(target),
            Self::UninstallPlugin(target) => Action::UninstallPlugin(target),
            Self::Logout => Action::Logout,
        }
    }
}

struct SideParent {
    thread_id: String,
    /// The turn the parent was still streaming when the fork opened. Returning
    /// hands it back so the spinner and `Esc` keep working on it.
    turn: Option<ParentTurn>,
}

struct ParentTurn {
    id: String,
    started_at: Instant,
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
    ThemePicker {
        theme_index: usize,
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
    McpForm(McpForm),
    McpApproval(McpApproval),
    McpUrl {
        id: Value,
        server_name: String,
        message: String,
        url: String,
    },
    Confirm {
        title: String,
        detail: Vec<String>,
        action: ConfirmedAction,
    },
    /// Claude-Code-style list that picks the sign-in flow before starting it.
    LoginMethodPicker {
        selected: usize,
    },
    /// Waiting on the browser half of `account/login/start`. Cleared by the
    /// `account/login/completed` notification or by cancelling.
    Login {
        login_id: String,
        waiting_on: Vec<String>,
    },
}

#[derive(Clone)]
struct SkillBinding {
    name: String,
    path: String,
    enabled: bool,
}

#[derive(Clone)]
struct MentionBinding {
    trigger: String,
    name: String,
    path: String,
}

struct McpApproval {
    id: Value,
    server_name: String,
    message: String,
    detail: Vec<String>,
    options: Vec<McpApprovalOption>,
    selected: usize,
}

struct McpApprovalOption {
    label: String,
    description: String,
    action: &'static str,
    persist: Option<&'static str>,
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

struct McpForm {
    id: Value,
    server_name: String,
    message: String,
    fields: Vec<McpField>,
    current: usize,
    editor: Editor,
    selected: usize,
    checked: Vec<bool>,
    content: Map<String, Value>,
    validation_error: Option<String>,
}

struct McpField {
    name: String,
    title: String,
    description: String,
    required: bool,
    default: Option<Value>,
    kind: McpFieldKind,
}

enum McpFieldKind {
    Text {
        format: Option<String>,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Number {
        integer: bool,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean,
    SingleSelect(Vec<McpOption>),
    MultiSelect {
        options: Vec<McpOption>,
        min_items: Option<usize>,
        max_items: Option<usize>,
    },
}

struct McpOption {
    value: String,
    label: String,
}

impl McpApproval {
    fn parse(id: Value, params: &Value) -> Option<Self> {
        if !mcp_schema_is_message_only(params.get("requestedSchema")) {
            return None;
        }

        let meta = params.get("_meta").and_then(Value::as_object);
        let is_tool_approval = meta
            .and_then(|meta| meta.get("codex_approval_kind"))
            .and_then(Value::as_str)
            == Some("mcp_tool_call");
        let mut options = vec![McpApprovalOption {
            label: "Allow".to_owned(),
            description: if is_tool_approval {
                "Run the tool and continue.".to_owned()
            } else {
                "Allow this request and continue.".to_owned()
            },
            action: "accept",
            persist: None,
        }];
        if mcp_persist_supported(meta, "session") {
            options.push(McpApprovalOption {
                label: "Allow for this session".to_owned(),
                description: if is_tool_approval {
                    "Run the tool and remember this choice for this session.".to_owned()
                } else {
                    "Allow this request for this session.".to_owned()
                },
                action: "accept",
                persist: Some("session"),
            });
        }
        if mcp_persist_supported(meta, "always") {
            options.push(McpApprovalOption {
                label: "Always allow".to_owned(),
                description: if is_tool_approval {
                    "Run the tool and remember this choice for future calls.".to_owned()
                } else {
                    "Always allow this request.".to_owned()
                },
                action: "accept",
                persist: Some("always"),
            });
        }
        if is_tool_approval {
            options.push(McpApprovalOption {
                label: "Cancel".to_owned(),
                description: "Cancel this tool call.".to_owned(),
                action: "cancel",
                persist: None,
            });
        } else {
            options.extend([
                McpApprovalOption {
                    label: "Deny".to_owned(),
                    description: "Decline this request and continue.".to_owned(),
                    action: "decline",
                    persist: None,
                },
                McpApprovalOption {
                    label: "Cancel".to_owned(),
                    description: "Cancel this request.".to_owned(),
                    action: "cancel",
                    persist: None,
                },
            ]);
        }

        Some(Self {
            id,
            server_name: params
                .get("serverName")
                .and_then(Value::as_str)
                .unwrap_or("MCP")
                .to_owned(),
            message: params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("이 요청을 허용할까요?")
                .to_owned(),
            detail: mcp_approval_detail(meta),
            options,
            selected: 0,
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Value> {
        match key.code {
            KeyCode::Esc => Some(mcp_elicitation_response("cancel", None)),
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
                None
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                let index = ch.to_digit(10)?.checked_sub(1)? as usize;
                if index >= self.options.len() {
                    return None;
                }
                self.selected = index;
                self.response()
            }
            KeyCode::Char('y') => {
                self.selected = 0;
                self.response()
            }
            KeyCode::Char('n') => {
                self.selected = self
                    .options
                    .iter()
                    .position(|option| matches!(option.action, "decline" | "cancel"))
                    .unwrap_or(self.options.len().saturating_sub(1));
                self.response()
            }
            KeyCode::Enter => self.response(),
            _ => None,
        }
    }

    fn response(&self) -> Option<Value> {
        let option = self.options.get(self.selected)?;
        let meta = option.persist.map(|persist| json!({ "persist": persist }));
        Some(mcp_elicitation_response_with_meta(
            option.action,
            None,
            meta,
        ))
    }
}

fn mcp_schema_is_message_only(schema: Option<&Value>) -> bool {
    schema.is_none_or(|schema| {
        schema.is_null()
            || schema.as_object().is_some_and(|schema| {
                schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(Map::is_empty)
            })
    })
}

fn mcp_persist_supported(meta: Option<&Map<String, Value>>, expected: &str) -> bool {
    match meta.and_then(|meta| meta.get("persist")) {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn mcp_approval_detail(meta: Option<&Map<String, Value>>) -> Vec<String> {
    let Some(meta) = meta else {
        return Vec::new();
    };
    let mut values = meta
        .get("tool_params_display")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let entry = entry.as_object()?;
                    let name = entry
                        .get("display_name")
                        .and_then(Value::as_str)
                        .or_else(|| entry.get("name").and_then(Value::as_str))?;
                    let value = entry.get("value")?;
                    Some(format!("{name}: {}", compact_json(value)))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if values.is_empty() {
        values = meta
            .get("tool_params")
            .and_then(Value::as_object)
            .map(|params| {
                params
                    .iter()
                    .map(|(name, value)| format!("{name}: {}", compact_json(value)))
                    .collect()
            })
            .unwrap_or_default();
        values.sort();
    }
    values.truncate(3);
    values
}

fn compact_json(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
        .chars()
        .take(80)
        .collect()
}

impl McpForm {
    fn parse(id: Value, params: &Value) -> Result<Self, String> {
        let server_name = params
            .get("serverName")
            .and_then(Value::as_str)
            .unwrap_or("MCP")
            .to_owned();
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("추가 입력이 필요합니다.")
            .to_owned();
        let schema = params
            .get("requestedSchema")
            .and_then(Value::as_object)
            .ok_or_else(|| "렌더링할 수 있는 MCP 폼 스키마가 없습니다.".to_owned())?;
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| "MCP 폼에 properties가 없습니다.".to_owned())?;
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut fields = Vec::with_capacity(properties.len());
        for (name, definition) in properties {
            fields.push(parse_mcp_field(
                name,
                definition,
                required.iter().any(|entry| entry.as_str() == Some(name)),
            )?);
        }
        let mut form = Self {
            id,
            server_name,
            message,
            fields,
            current: 0,
            editor: Editor::default(),
            selected: 0,
            checked: Vec::new(),
            content: Map::new(),
            validation_error: None,
        };
        form.reset_controls();
        Ok(form)
    }

    fn reset_controls(&mut self) {
        self.editor = Editor::default();
        self.selected = 0;
        self.checked.clear();
        let Some(field) = self.fields.get(self.current) else {
            return;
        };
        match &field.kind {
            McpFieldKind::Text { .. } | McpFieldKind::Number { .. } => {
                if let Some(default) = field.default.as_ref() {
                    let value = default
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| default.to_string());
                    self.editor.set_text(&value);
                }
            }
            McpFieldKind::Boolean => {
                self.selected = match field.default.as_ref().and_then(Value::as_bool) {
                    Some(false) if field.required => 0,
                    Some(true) if field.required => 1,
                    Some(false) => 1,
                    Some(true) => 2,
                    None => 0,
                };
            }
            McpFieldKind::SingleSelect(options) => {
                let offset = usize::from(!field.required);
                if let Some(default) = field.default.as_ref().and_then(Value::as_str) {
                    self.selected = options
                        .iter()
                        .position(|option| option.value == default)
                        .map(|index| index + offset)
                        .unwrap_or(0);
                }
            }
            McpFieldKind::MultiSelect { options, .. } => {
                let defaults = field
                    .default
                    .as_ref()
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                self.checked = options
                    .iter()
                    .map(|option| {
                        defaults
                            .iter()
                            .any(|value| value.as_str() == Some(&option.value))
                    })
                    .collect();
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Value> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.code == KeyCode::Esc {
            return Some(mcp_elicitation_response("cancel", None));
        }
        if alt && key.code == KeyCode::Char('d') {
            return Some(mcp_elicitation_response("decline", None));
        }

        let Some(field) = self.fields.get(self.current) else {
            return Some(mcp_elicitation_response(
                "accept",
                Some(Value::Object(std::mem::take(&mut self.content))),
            ));
        };
        match &field.kind {
            McpFieldKind::Text { .. } | McpFieldKind::Number { .. } => match key.code {
                KeyCode::Enter => return self.commit_current(),
                KeyCode::Backspace if ctrl => self.editor.delete_word_left(),
                KeyCode::Backspace => self.editor.backspace(),
                KeyCode::Delete => self.editor.delete(),
                KeyCode::Left if ctrl || alt => self.editor.move_word_left(),
                KeyCode::Right if ctrl || alt => self.editor.move_word_right(),
                KeyCode::Left => self.editor.move_left(),
                KeyCode::Right => self.editor.move_right(),
                KeyCode::Home => self.editor.move_home(),
                KeyCode::End => self.editor.move_end(),
                KeyCode::Char('w') if ctrl => self.editor.delete_word_left(),
                KeyCode::Char('k') if ctrl => self.editor.delete_to_line_end(),
                KeyCode::Char('u') if ctrl => self.editor.delete_to_line_start(),
                KeyCode::Char('y') if ctrl => self.editor.yank(),
                KeyCode::Char(ch) if !ctrl && !alt => self.editor.insert(ch),
                _ => {}
            },
            McpFieldKind::Boolean => match key.code {
                KeyCode::Up | KeyCode::Left => self.selected = 0,
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    let maximum = if field.required { 1 } else { 2 };
                    self.selected = (self.selected + 1).min(maximum);
                }
                KeyCode::Char(' ') => {
                    let count = if field.required { 2 } else { 3 };
                    self.selected = (self.selected + 1) % count;
                }
                KeyCode::Enter => return self.commit_current(),
                _ => {}
            },
            McpFieldKind::SingleSelect(options) => match key.code {
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    let count = options.len() + usize::from(!field.required);
                    self.selected = (self.selected + 1).min(count.saturating_sub(1));
                }
                KeyCode::Enter => return self.commit_current(),
                _ => {}
            },
            McpFieldKind::MultiSelect { options, .. } => match key.code {
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    self.selected = (self.selected + 1).min(options.len().saturating_sub(1));
                }
                KeyCode::Char(' ') => {
                    if let Some(checked) = self.checked.get_mut(self.selected) {
                        *checked = !*checked;
                    }
                }
                KeyCode::Enter => return self.commit_current(),
                _ => {}
            },
        }
        None
    }

    fn commit_current(&mut self) -> Option<Value> {
        let field = &self.fields[self.current];
        match mcp_field_value(field, &mut self.editor, self.selected, &self.checked) {
            Ok(Some(value)) => {
                self.content.insert(field.name.clone(), value);
            }
            Ok(None) => {
                self.content.remove(&field.name);
            }
            Err(error) => {
                self.validation_error = Some(error);
                return None;
            }
        }
        self.validation_error = None;
        self.current += 1;
        if self.current == self.fields.len() {
            return Some(mcp_elicitation_response(
                "accept",
                Some(Value::Object(std::mem::take(&mut self.content))),
            ));
        }
        self.reset_controls();
        None
    }
}

fn parse_mcp_field(name: &str, definition: &Value, required: bool) -> Result<McpField, String> {
    let object = definition
        .as_object()
        .ok_or_else(|| format!("MCP 필드 '{name}'의 스키마가 올바르지 않습니다."))?;
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_owned();
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let default = object
        .get("default")
        .filter(|value| !value.is_null())
        .cloned();
    let field_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    let kind = if field_type == "array" {
        let items = object
            .get("items")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("MCP 다중 선택 필드 '{name}'에 items가 없습니다."))?;
        McpFieldKind::MultiSelect {
            options: parse_mcp_options(items)
                .ok_or_else(|| format!("MCP 필드 '{name}'의 선택 항목을 해석할 수 없습니다."))?,
            min_items: object
                .get("minItems")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            max_items: object
                .get("maxItems")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        }
    } else if let Some(options) = parse_mcp_options(object) {
        McpFieldKind::SingleSelect(options)
    } else {
        match field_type {
            "string" => McpFieldKind::Text {
                format: object
                    .get("format")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                min_length: object
                    .get("minLength")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                max_length: object
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
            },
            "number" | "integer" => McpFieldKind::Number {
                integer: field_type == "integer",
                minimum: object.get("minimum").and_then(Value::as_f64),
                maximum: object.get("maximum").and_then(Value::as_f64),
            },
            "boolean" => McpFieldKind::Boolean,
            unsupported => {
                return Err(format!(
                    "MCP 필드 '{name}'의 형식 '{unsupported}'은 지원하지 않습니다."
                ));
            }
        }
    };
    Ok(McpField {
        name: name.to_owned(),
        title,
        description,
        required,
        default,
        kind,
    })
}

fn parse_mcp_options(object: &Map<String, Value>) -> Option<Vec<McpOption>> {
    if let Some(entries) = object.get("oneOf").and_then(Value::as_array) {
        return Some(
            entries
                .iter()
                .filter_map(|entry| {
                    Some(McpOption {
                        value: entry.get("const")?.as_str()?.to_owned(),
                        label: entry
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| {
                                entry.get("const").and_then(Value::as_str).unwrap_or("")
                            })
                            .to_owned(),
                    })
                })
                .collect(),
        )
        .filter(|options: &Vec<McpOption>| !options.is_empty());
    }
    if let Some(entries) = object.get("anyOf").and_then(Value::as_array) {
        return Some(
            entries
                .iter()
                .filter_map(|entry| {
                    Some(McpOption {
                        value: entry.get("const")?.as_str()?.to_owned(),
                        label: entry
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| {
                                entry.get("const").and_then(Value::as_str).unwrap_or("")
                            })
                            .to_owned(),
                    })
                })
                .collect(),
        )
        .filter(|options: &Vec<McpOption>| !options.is_empty());
    }
    let values = object.get("enum").and_then(Value::as_array)?;
    let names = object.get("enumNames").and_then(Value::as_array);
    Some(
        values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let value = value.as_str()?.to_owned();
                let label = names
                    .and_then(|names| names.get(index))
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_owned();
                Some(McpOption { value, label })
            })
            .collect(),
    )
    .filter(|options: &Vec<McpOption>| !options.is_empty())
}

fn mcp_field_value(
    field: &McpField,
    editor: &mut Editor,
    selected: usize,
    checked: &[bool],
) -> Result<Option<Value>, String> {
    match &field.kind {
        McpFieldKind::Text {
            format,
            min_length,
            max_length,
        } => {
            let value = editor.text().to_owned();
            if value.is_empty() {
                return if field.required {
                    Err(format!("{}은(는) 필수 항목입니다.", field.title))
                } else {
                    Ok(None)
                };
            }
            let length = value.chars().count();
            if min_length.is_some_and(|minimum| length < minimum) {
                return Err(format!(
                    "{}은(는) 최소 {}자여야 합니다.",
                    field.title,
                    min_length.unwrap_or_default()
                ));
            }
            if max_length.is_some_and(|maximum| length > maximum) {
                return Err(format!(
                    "{}은(는) 최대 {}자까지 입력할 수 있습니다.",
                    field.title,
                    max_length.unwrap_or_default()
                ));
            }
            if format.as_deref() == Some("email") && !looks_like_email(&value) {
                return Err("올바른 이메일 주소를 입력하세요.".to_owned());
            }
            if format.as_deref() == Some("uri") && !looks_like_uri(&value) {
                return Err("올바른 URI를 입력하세요.".to_owned());
            }
            if format.as_deref() == Some("date")
                && chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d").is_err()
            {
                return Err("날짜를 YYYY-MM-DD 형식으로 입력하세요.".to_owned());
            }
            if format.as_deref() == Some("date-time")
                && chrono::DateTime::parse_from_rfc3339(&value).is_err()
            {
                return Err("날짜와 시간을 RFC 3339 형식으로 입력하세요.".to_owned());
            }
            Ok(Some(Value::String(value)))
        }
        McpFieldKind::Number {
            integer,
            minimum,
            maximum,
        } => {
            let input = editor.text();
            let raw = input.trim();
            if raw.is_empty() {
                return if field.required {
                    Err(format!("{}은(는) 필수 항목입니다.", field.title))
                } else {
                    Ok(None)
                };
            }
            let number = raw
                .parse::<f64>()
                .map_err(|_| "숫자를 입력하세요.".to_owned())?;
            if *integer && number.fract() != 0.0 {
                return Err("정수를 입력하세요.".to_owned());
            }
            if minimum.is_some_and(|minimum| number < minimum) {
                return Err(format!(
                    "{} 이상을 입력하세요.",
                    minimum.unwrap_or_default()
                ));
            }
            if maximum.is_some_and(|maximum| number > maximum) {
                return Err(format!(
                    "{} 이하를 입력하세요.",
                    maximum.unwrap_or_default()
                ));
            }
            if *integer {
                let integer = raw
                    .parse::<i64>()
                    .map_err(|_| "지원 범위 안의 정수를 입력하세요.".to_owned())?;
                Ok(Some(Value::Number(integer.into())))
            } else {
                serde_json::Number::from_f64(number)
                    .map(Value::Number)
                    .map(Some)
                    .ok_or_else(|| "유효한 숫자를 입력하세요.".to_owned())
            }
        }
        McpFieldKind::Boolean if !field.required && selected == 0 => Ok(None),
        McpFieldKind::Boolean => Ok(Some(Value::Bool(if field.required {
            selected == 1
        } else {
            selected == 2
        }))),
        McpFieldKind::SingleSelect(options) => {
            let offset = usize::from(!field.required);
            if selected < offset {
                return Ok(None);
            }
            options
                .get(selected - offset)
                .map(|option| Some(Value::String(option.value.clone())))
                .ok_or_else(|| "선택 항목이 없습니다.".to_owned())
        }
        McpFieldKind::MultiSelect {
            options,
            min_items,
            max_items,
        } => {
            let values = options
                .iter()
                .zip(checked)
                .filter(|(_, checked)| **checked)
                .map(|(option, _)| Value::String(option.value.clone()))
                .collect::<Vec<_>>();
            if field.required && values.is_empty() {
                return Err(format!("{}에서 하나 이상 선택하세요.", field.title));
            }
            if min_items.is_some_and(|minimum| values.len() < minimum) {
                return Err(format!(
                    "최소 {}개를 선택하세요.",
                    min_items.unwrap_or_default()
                ));
            }
            if max_items.is_some_and(|maximum| values.len() > maximum) {
                return Err(format!(
                    "최대 {}개까지 선택할 수 있습니다.",
                    max_items.unwrap_or_default()
                ));
            }
            if values.is_empty() && !field.required {
                Ok(None)
            } else {
                Ok(Some(Value::Array(values)))
            }
        }
    }
}

fn mcp_elicitation_response(action: &str, content: Option<Value>) -> Value {
    mcp_elicitation_response_with_meta(action, content, None)
}

fn mcp_elicitation_response_with_meta(
    action: &str,
    content: Option<Value>,
    meta: Option<Value>,
) -> Value {
    json!({
        "action": action,
        "content": content,
        "_meta": meta
    })
}

fn looks_like_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn looks_like_uri(value: &str) -> bool {
    value
        .split_once(':')
        .is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty())
}

fn parse_skill_bindings(response: &Value) -> Vec<SkillBinding> {
    response
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
        .filter_map(|skill| {
            Some(SkillBinding {
                name: skill.get("name")?.as_str()?.to_owned(),
                path: skill.get("path")?.as_str()?.to_owned(),
                enabled: skill
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect()
}

fn parse_plugin_mentions(response: &Value) -> Vec<MentionBinding> {
    let mut mentions = Vec::new();
    for marketplace in response
        .get("marketplaces")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if marketplace.get("name").and_then(Value::as_str).is_none() {
            continue;
        }
        for plugin in marketplace
            .get("plugins")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if plugin.get("installed").and_then(Value::as_bool) != Some(true)
                || plugin.get("enabled").and_then(Value::as_bool) == Some(false)
            {
                continue;
            }
            let Some(plugin_name) = plugin.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(plugin_id) = plugin.get("id").and_then(Value::as_str) else {
                continue;
            };
            let display_name = plugin
                .get("interface")
                .and_then(|interface| interface.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or(plugin_name);
            let path = format!("plugin://{plugin_id}");
            let mut triggers = vec![slugify_mention(plugin_name)];
            let display_trigger = slugify_mention(display_name);
            if !triggers.contains(&display_trigger) {
                triggers.push(display_trigger);
            }
            mentions.extend(triggers.into_iter().map(|trigger| MentionBinding {
                trigger,
                name: display_name.to_owned(),
                path: path.clone(),
            }));
        }
    }
    mentions
}

fn parse_app_mentions(response: &Value) -> Vec<MentionBinding> {
    let mut mentions = Vec::new();
    for app in response
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if app.get("isAccessible").and_then(Value::as_bool) != Some(true)
            || app.get("isEnabled").and_then(Value::as_bool) == Some(false)
        {
            continue;
        }
        let Some(id) = app.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = app.get("name").and_then(Value::as_str).unwrap_or(id);
        let path = format!("app://{id}");
        let mut triggers = vec![slugify_mention(id)];
        let name_trigger = slugify_mention(name);
        if !triggers.contains(&name_trigger) {
            triggers.push(name_trigger);
        }
        mentions.extend(triggers.into_iter().map(|trigger| MentionBinding {
            trigger,
            name: name.to_owned(),
            path: path.clone(),
        }));
    }
    mentions
}

fn slugify_mention(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(ch.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    result
}

fn mention_triggers(text: &str) -> Vec<String> {
    let mut triggers = Vec::new();
    let chars = text.char_indices().collect::<Vec<_>>();
    for (position, (_, ch)) in chars.iter().enumerate() {
        if !matches!(ch, '$' | '@') {
            continue;
        }
        if position
            .checked_sub(1)
            .and_then(|index| chars.get(index))
            .is_some_and(|(_, previous)| {
                previous.is_ascii_alphanumeric() || matches!(previous, '_' | '-' | '.')
            })
        {
            continue;
        }
        let start = chars
            .get(position + 1)
            .map(|(index, _)| *index)
            .unwrap_or(text.len());
        let end = chars[position + 1..]
            .iter()
            .find(|(_, candidate)| {
                !(candidate.is_ascii_alphanumeric() || matches!(candidate, '-' | '_' | ':' | '.'))
            })
            .map(|(index, _)| *index)
            .unwrap_or(text.len());
        if end > start {
            triggers.push(text[start..end].to_owned());
        }
    }
    triggers
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
            // The placeholder already says what the field is for; labelling the
            // rule above it as well just says it twice.
            input_label: "",
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
    /// Tokens the *current* prompt occupies, not the thread's running tally.
    /// The tally climbs past the window on every turn and is not a context gauge.
    context_tokens: u64,
    /// The running tally, which is what billing counts.
    token_totals: TokenTotals,
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
    account_plan: AccountPlan,
    /// Set when a login lands, so the event loop re-reads the account over RPC.
    account_refresh_due: bool,
    skills: Vec<SkillBinding>,
    mentions: Vec<MentionBinding>,
    app_mentions: Vec<MentionBinding>,
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
            context_tokens: 0,
            token_totals: TokenTotals::default(),
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
            account_plan: AccountPlan::default(),
            account_refresh_due: false,
            skills: Vec::new(),
            mentions: Vec::new(),
            app_mentions: Vec::new(),
        }
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.models.get(self.selected_model)
    }

    pub fn set_account_plan(&mut self, plan: AccountPlan) {
        self.account_plan = plan;
    }

    pub fn update_skills(&mut self, response: &Value) {
        self.skills = parse_skill_bindings(response);
    }

    pub fn update_plugins(&mut self, response: &Value) {
        self.mentions = parse_plugin_mentions(response);
    }

    pub fn update_apps(&mut self, response: &Value) {
        self.app_mentions = parse_app_mentions(response);
    }

    pub fn turn_input(&self, text: String) -> Vec<Value> {
        let triggers = mention_triggers(&text);
        let mut input = vec![json!({
            "type": "text",
            "text": text,
            "text_elements": []
        })];
        let mut added_paths = Vec::new();
        for skill in &self.skills {
            if skill.enabled
                && triggers
                    .iter()
                    .any(|trigger| trigger.eq_ignore_ascii_case(&skill.name))
                && !added_paths.contains(&skill.path)
            {
                input.push(json!({
                    "type": "skill",
                    "name": skill.name,
                    "path": skill.path
                }));
                added_paths.push(skill.path.clone());
            }
        }
        for mention in &self.mentions {
            if triggers
                .iter()
                .any(|trigger| trigger.eq_ignore_ascii_case(&mention.trigger))
                && !added_paths.contains(&mention.path)
            {
                input.push(json!({
                    "type": "mention",
                    "name": mention.name,
                    "path": mention.path
                }));
                added_paths.push(mention.path.clone());
            }
        }
        for mention in &self.app_mentions {
            if triggers
                .iter()
                .any(|trigger| trigger.eq_ignore_ascii_case(&mention.trigger))
                && !added_paths.contains(&mention.path)
            {
                input.push(json!({
                    "type": "mention",
                    "name": mention.name,
                    "path": mention.path
                }));
                added_paths.push(mention.path.clone());
            }
        }
        input
    }

    pub fn confirm_plugin_install(
        &mut self,
        target: PluginInstallTarget,
        marketplace: &str,
        description: Option<&str>,
        disclosure: Vec<String>,
    ) {
        let mut detail = vec![
            format!("Plugin: {}", target.plugin_name),
            format!("Marketplace: {marketplace}"),
        ];
        if let Some(description) = description.filter(|text| !text.is_empty()) {
            detail.push(description.to_owned());
        }
        detail.extend(disclosure);
        detail.push("설치하면 포함된 Skill, MCP 서버와 Hook이 Codex에 추가됩니다.".to_owned());
        self.pending = Some(PendingInteraction::Confirm {
            title: "플러그인을 설치할까요?".to_owned(),
            detail,
            action: ConfirmedAction::InstallPlugin(target),
        });
    }

    /// Opens the modal that waits for the browser half of the OAuth flow.
    pub fn begin_login(&mut self, login_id: String, auth_url: String) {
        self.commit_welcome_card();
        // OAuth URLs run past 400 characters. Folding one inside a bordered modal
        // swallows the screen, so the full URL lives in the scrollback instead.
        self.committed
            .push(Block::new(BlockKind::System, "Sign-in URL", auth_url));
        self.pending = Some(PendingInteraction::Login {
            login_id,
            waiting_on: vec![
                "브라우저에서 로그인을 완료하세요.".to_owned(),
                "열리지 않으면 위 Sign-in URL을 사용하세요.".to_owned(),
            ],
        });
    }

    /// Device-code variant: the user types `user_code` on another device.
    pub fn begin_device_login(&mut self, login_id: String, url: String, user_code: String) {
        self.commit_welcome_card();
        self.committed.push(Block::new(
            BlockKind::System,
            "Sign-in URL",
            format!("{url}\ncode: {user_code}"),
        ));
        self.pending = Some(PendingInteraction::Login {
            login_id,
            waiting_on: vec![
                format!("코드: {user_code}"),
                "위 URL을 열고 이 코드를 입력하세요.".to_owned(),
            ],
        });
    }

    /// Opens the sign-in method list.
    pub fn open_login_picker(&mut self) {
        self.pending = Some(PendingInteraction::LoginMethodPicker { selected: 0 });
    }

    /// Login id of an in-flight `/login`, so a caller can cancel it.
    pub fn active_login_id(&self) -> Option<&str> {
        match self.pending.as_ref() {
            Some(PendingInteraction::Login { login_id, .. }) => Some(login_id.as_str()),
            _ => None,
        }
    }

    pub fn finish_login(&mut self, success: bool, error: Option<&str>) {
        if matches!(self.pending, Some(PendingInteraction::Login { .. })) {
            self.pending = None;
        }
        if success {
            self.push_notice(
                BlockKind::System,
                "로그인 완료",
                "계정 정보를 갱신했습니다.",
            );
        } else {
            self.push_notice(
                BlockKind::Error,
                "로그인 실패",
                error.unwrap_or("알 수 없는 오류로 로그인이 완료되지 않았습니다."),
            );
        }
    }

    pub fn cancel_login_notice(&mut self) {
        self.pending = None;
        self.push_notice(BlockKind::Warning, "로그인 취소", "로그인을 중단했습니다.");
    }

    pub fn confirm_logout(&mut self) {
        self.pending = Some(PendingInteraction::Confirm {
            title: "로그아웃할까요?".to_owned(),
            detail: vec![
                format!("Account: {}", self.account),
                "다시 사용하려면 /login으로 재인증해야 합니다.".to_owned(),
            ],
            action: ConfirmedAction::Logout,
        });
    }

    /// Clears the cached identity so the welcome panel stops advertising a
    /// session that no longer has credentials.
    pub fn apply_logout(&mut self) {
        self.account = "signed out".to_owned();
        self.account_plan = AccountPlan::default();
        self.push_notice(
            BlockKind::Warning,
            "로그아웃",
            "계정 연결을 해제했습니다. /login으로 다시 로그인하세요.",
        );
    }

    /// Replaces the cached identity after a successful `/login`.
    pub fn set_account(&mut self, account: String) {
        self.account = account;
    }

    /// True once per account change, so the event loop refreshes over RPC exactly once.
    pub fn take_account_refresh(&mut self) -> bool {
        std::mem::take(&mut self.account_refresh_due)
    }

    pub fn confirm_plugin_uninstall(&mut self, target: PluginUninstallTarget) {
        self.pending = Some(PendingInteraction::Confirm {
            title: "플러그인을 제거할까요?".to_owned(),
            detail: vec![
                format!("Plugin: {}", target.display_name),
                "포함된 Skill과 MCP 연결이 새 세션부터 제거됩니다.".to_owned(),
            ],
            action: ConfirmedAction::UninstallPlugin(target),
        });
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
            fast_mode: self.effective_fast_mode(),
            cost: self.estimated_cost(),
        }
    }

    /// Estimated spend for the thread so far. `None` before the first turn
    /// reports usage, or when the model has no published rate.
    fn estimated_cost(&self) -> Option<String> {
        if self.token_totals.is_empty() {
            return None;
        }
        pricing::estimate_usd(self.selected_model_name(), self.token_totals)
            .map(pricing::format_usd)
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
        self.set_composer_notice(format!("Copied {count} chars to clipboard"));
    }

    /// One-off events (skills reloaded, model rerouted, …) share the composer
    /// notice slot with the copy message: it sits where the eye already is and
    /// `tick` clears it after 1.4s. The status line is for standing state only,
    /// so nothing parks there waiting for a new thread to wipe it.
    fn set_composer_notice(&mut self, message: String) {
        self.composer_notice = Some((message, Instant::now()));
    }

    pub fn side_parent_thread_id(&self) -> Option<&str> {
        self.side_parent
            .as_ref()
            .map(|parent| parent.thread_id.as_str())
    }

    /// Lifts the parent's still-live turn out before `thread/resume` rebuilds
    /// the state from scratch. `None` once the turn has ended on its own.
    pub fn take_side_parent_turn(&mut self) -> Option<(String, Instant)> {
        self.side_parent
            .as_mut()?
            .turn
            .take()
            .map(|turn| (turn.id, turn.started_at))
    }

    /// Puts a turn that outlived a side conversation back in front of the user.
    pub fn restore_turn(&mut self, turn: Option<(String, Instant)>) {
        let Some((turn_id, started_at)) = turn else {
            return;
        };
        self.turn_id = Some(turn_id);
        self.turn_started_at = Some(started_at);
        self.busy = true;
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
            turn: self
                .turn_id
                .clone()
                .filter(|_| self.busy)
                .map(|id| ParentTurn {
                    id,
                    started_at: self.turn_started_at.unwrap_or_else(Instant::now),
                }),
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
        self.context_tokens = 0;
        self.token_totals = TokenTotals::default();
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

    /// Announce a newer published release above the composer history.
    pub fn push_update_available(&mut self, latest: &str) {
        self.push_notice(
            BlockKind::Update,
            "Update Available",
            format!("New version {latest} is available. Run: dvz update"),
        );
    }

    /// Notice painted before a slow round-trip. The event loop cannot redraw
    /// while an action awaits, so callers set this and draw once up front.
    pub fn set_waiting_notice(&mut self, message: impl Into<String>) {
        self.set_composer_notice(message.into());
    }

    /// Brings the welcome panel back after the screen is wiped, so a cleared
    /// terminal looks like a fresh start instead of a bare composer.
    pub fn reset_welcome(&mut self) {
        self.show_welcome = true;
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
            welcome: self.show_welcome.then(|| self.welcome_view()),
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
            Some(PendingInteraction::McpForm(form))
                if form.fields.get(form.current).is_some_and(|field| {
                    matches!(
                        &field.kind,
                        McpFieldKind::Text { .. } | McpFieldKind::Number { .. }
                    )
                }) =>
            {
                form.editor.insert_str(text);
            }
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
            if method == "mcpServer/elicitation/request" {
                return Action::RpcResponse {
                    id,
                    result: mcp_elicitation_response("cancel", None),
                };
            }
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
                let mode = params.get("mode").and_then(Value::as_str).unwrap_or("form");
                if mode == "url" {
                    let Some(url) = params.get("url").and_then(Value::as_str) else {
                        return Action::RpcResponse {
                            id,
                            result: mcp_elicitation_response("decline", None),
                        };
                    };
                    self.pending = Some(PendingInteraction::McpUrl {
                        id,
                        server_name: params
                            .get("serverName")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP")
                            .to_owned(),
                        message: params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("브라우저에서 계속하세요.")
                            .to_owned(),
                        url: url.to_owned(),
                    });
                    return Action::None;
                }
                if mcp_schema_is_message_only(params.get("requestedSchema")) {
                    let meta = params.get("_meta").and_then(Value::as_object);
                    if meta
                        .and_then(|meta| meta.get("codex_approval_kind"))
                        .and_then(Value::as_str)
                        == Some("tool_suggestion")
                        && let Some(url) = meta
                            .and_then(|meta| meta.get("install_url"))
                            .and_then(Value::as_str)
                    {
                        self.pending = Some(PendingInteraction::McpUrl {
                            id,
                            server_name: meta
                                .and_then(|meta| meta.get("tool_name"))
                                .and_then(Value::as_str)
                                .unwrap_or("Codex App")
                                .to_owned(),
                            message: meta
                                .and_then(|meta| meta.get("suggest_reason"))
                                .and_then(Value::as_str)
                                .or_else(|| params.get("message").and_then(Value::as_str))
                                .unwrap_or("브라우저에서 연결을 완료하세요.")
                                .to_owned(),
                            url: url.to_owned(),
                        });
                        return Action::None;
                    }
                    if let Some(approval) = McpApproval::parse(id.clone(), params) {
                        self.pending = Some(PendingInteraction::McpApproval(approval));
                        return Action::None;
                    }
                }
                match McpForm::parse(id.clone(), params) {
                    Ok(form) => {
                        self.pending = Some(PendingInteraction::McpForm(form));
                        Action::None
                    }
                    Err(error) => {
                        self.committed.push(Block::new(
                            BlockKind::Warning,
                            "MCP 폼을 표시할 수 없음",
                            format!("{error}\n서버 요청을 안전하게 거부했습니다."),
                        ));
                        Action::RpcResponse {
                            id,
                            result: mcp_elicitation_response("decline", None),
                        }
                    }
                }
            }
            _ => Action::RpcError {
                id,
                message: format!("지원하지 않는 서버 요청: {method}"),
            },
        }
    }

    fn clear_resolved_server_request(&mut self, request_id: &Value) {
        let matches = match self.pending.as_ref() {
            Some(PendingInteraction::Approval { id, .. })
            | Some(PendingInteraction::UserInput { id, .. })
            | Some(PendingInteraction::McpUrl { id, .. }) => id == request_id,
            Some(PendingInteraction::McpForm(form)) => &form.id == request_id,
            Some(PendingInteraction::McpApproval(approval)) => &approval.id == request_id,
            _ => false,
        };
        if matches {
            self.pending = None;
        }
    }

    /// A parent turn keeps streaming behind a `/btw` fork. Its output stays out
    /// of the fork's view, but the end of the turn has to land somewhere so we
    /// do not restore a spinner for a turn that already finished.
    fn note_background_turn(&mut self, thread_id: &str, method: &str) {
        if method != "turn/completed" {
            return;
        }
        if let Some(parent) = self
            .side_parent
            .as_mut()
            .filter(|parent| parent.thread_id == thread_id)
        {
            parent.turn = None;
        }
    }

    pub fn handle_notification(&mut self, method: &str, params: &Value) {
        if let Some(thread_id) = params
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|thread_id| *thread_id != self.thread_id)
        {
            self.note_background_turn(thread_id, method);
            return;
        }
        match method {
            "serverRequest/resolved" => {
                if let Some(request_id) = params.get("requestId") {
                    self.clear_resolved_server_request(request_id);
                }
            }
            "account/login/completed" => {
                let success = params
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let error = params.get("error").and_then(Value::as_str);
                self.finish_login(success, error);
                self.account_refresh_due |= success;
            }
            // Plan or auth mode changed underneath us; pull the fresh values.
            "account/updated" => self.account_refresh_due = true,
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
                    // `ThreadTokenUsage.total` accumulates every turn of the
                    // thread, so it runs past the window and cannot gauge the
                    // context. `last` is what the current prompt occupies.
                    self.context_tokens = usage
                        .get("last")
                        .and_then(|last| last.get("totalTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(self.context_tokens);
                    // Billing counts every turn, so the tally is the right input
                    // for the traffic and cost figures on the composer rule.
                    if let Some(total) = usage.get("total") {
                        self.token_totals = TokenTotals::from_breakdown(total);
                    }
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
                    self.set_composer_notice(format!("{from} → {to}로 전환됨"));
                }
            }
            "skills/changed" => {
                self.set_composer_notice("Skills refreshed".to_owned());
            }
            "app/list/updated" => {
                self.update_apps(params);
                self.set_composer_notice("Apps refreshed".to_owned());
            }
            "mcpServer/oauthLogin/completed" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("MCP");
                let success = params
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.committed.push(Block::new(
                    if success {
                        BlockKind::System
                    } else {
                        BlockKind::Error
                    },
                    if success {
                        "MCP connected"
                    } else {
                        "MCP connection failed"
                    },
                    if success {
                        format!("{name} 인증이 완료되었습니다.")
                    } else {
                        params
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("OAuth 인증을 완료하지 못했습니다.")
                            .to_owned()
                    },
                ));
            }
            "mcpServer/startupStatus/updated" => {
                let status = params
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if status == "failed" {
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("MCP");
                    let reconnect = params.get("failureReason").and_then(Value::as_str)
                        == Some("reauthenticationRequired");
                    let detail = if reconnect {
                        format!("인증이 만료되었습니다. /mcp login {name}")
                    } else {
                        params
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP 서버를 시작하지 못했습니다.")
                            .to_owned()
                    };
                    self.committed.push(Block::new(
                        BlockKind::Warning,
                        format!("{name} unavailable"),
                        detail,
                    ));
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
                    "/model [MODEL] [EFFORT]  모델과 effort 선택\n/fast  빠른 서비스 티어 전환\n/effort [LEVEL]  추론 수준\n/theme [minimal|soft|dark]  화면 테마\n/mcp [login NAME]  MCP 서버와 OAuth\n/plugins [install|uninstall|enable|disable NAME]  플러그인 관리\n/skills [enable|disable NAME]  Skill 관리\n/apps [enable|disable NAME]  App 관리\n/btw [MESSAGE]  임시 사이드 대화\n/compact  컨텍스트 압축\n/copy  마지막 답변 복사\n/diff  git diff 표시\n/resume [SESSION]  이전 세션 선택\n/continue  /resume 별칭\n/new  새 대화\n/login  ChatGPT 계정 로그인\n/logout  계정 연결 해제\n/status  현재 설정\n/usage  사용 한도\n/clear  화면 정리\n/quit  종료\n\n$skill-name, $app-name 또는 @plugin-name  명시적으로 사용\nEsc 또는 Ctrl+C  실행 중단\nShift+Tab  권한 모드 전환 (Read Only / Default / Full Access)\nCtrl+Enter / Shift+Enter  줄바꿈",
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
            "/theme" if parts.len() == 1 => {
                self.pending = Some(PendingInteraction::ThemePicker {
                    theme_index: theme::current().index(),
                });
                Action::None
            }
            "/theme" if parts.len() == 2 => {
                let Some(selected) = ThemeKind::parse(parts[1]) else {
                    self.committed.push(Block::new(
                        BlockKind::Error,
                        "지원하지 않는 테마",
                        "minimal, soft, dark 중 하나를 선택하세요.",
                    ));
                    return Action::None;
                };
                self.apply_theme(selected)
            }
            "/mcp" if parts.len() == 1 => Action::ShowMcp,
            "/mcp" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("login") => {
                Action::McpLogin(parts[2..].join(" "))
            }
            "/mcp" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/mcp 또는 /mcp login SERVER",
                ));
                Action::None
            }
            "/login" => {
                self.open_login_picker();
                Action::None
            }
            "/logout" => {
                self.confirm_logout();
                Action::None
            }
            "/plugins" if parts.len() == 1 => Action::ShowPlugins,
            "/plugins" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("install") => {
                Action::PreparePluginInstall(parts[2..].join(" "))
            }
            "/plugins" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("uninstall") => {
                Action::PreparePluginUninstall(parts[2..].join(" "))
            }
            "/plugins" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("enable") => {
                Action::SetPlugin {
                    query: parts[2..].join(" "),
                    enabled: true,
                }
            }
            "/plugins" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("disable") => {
                Action::SetPlugin {
                    query: parts[2..].join(" "),
                    enabled: false,
                }
            }
            "/plugins" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/plugins 또는 /plugins install|uninstall|enable|disable NAME",
                ));
                Action::None
            }
            "/skills" if parts.len() == 1 => Action::ShowSkills,
            "/skills" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("enable") => {
                Action::SetSkill {
                    name: parts[2..].join(" "),
                    enabled: true,
                }
            }
            "/skills" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("disable") => {
                Action::SetSkill {
                    name: parts[2..].join(" "),
                    enabled: false,
                }
            }
            "/skills" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/skills 또는 /skills enable|disable NAME",
                ));
                Action::None
            }
            "/apps" if parts.len() == 1 => Action::ShowApps,
            "/apps" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("enable") => {
                Action::SetApp {
                    query: parts[2..].join(" "),
                    enabled: true,
                }
            }
            "/apps" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("disable") => {
                Action::SetApp {
                    query: parts[2..].join(" "),
                    enabled: false,
                }
            }
            "/apps" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/apps 또는 /apps enable|disable NAME",
                ));
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
            // `/btw` is for asking something *while* the main turn runs, so it
            // never waits for the turn to finish.
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
                        "thread: {}\nmodel: {model}\neffort: {}\ntheme: {}\npermissions: {} ({})\ncwd: {}",
                        self.thread_id,
                        self.selected_effort,
                        theme::current().display_name(),
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
            PendingInteraction::ThemePicker { mut theme_index } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up | KeyCode::Left => {
                        theme_index = theme_index.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        theme_index = (theme_index + 1).min(ThemeKind::ALL.len() - 1);
                    }
                    KeyCode::Char(ch) if ('1'..='3').contains(&ch) => {
                        let selected = ThemeKind::ALL[ch.to_digit(10).unwrap_or(1) as usize - 1];
                        return self.apply_theme(selected);
                    }
                    KeyCode::Enter => {
                        return self.apply_theme(ThemeKind::ALL[theme_index]);
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::ThemePicker { theme_index });
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
            PendingInteraction::McpApproval(mut approval) => {
                let id = approval.id.clone();
                if let Some(result) = approval.handle_key(key) {
                    Action::RpcResponse { id, result }
                } else {
                    self.pending = Some(PendingInteraction::McpApproval(approval));
                    Action::None
                }
            }
            PendingInteraction::McpForm(mut form) => {
                let id = form.id.clone();
                if let Some(result) = form.handle_key(key) {
                    Action::RpcResponse { id, result }
                } else {
                    self.pending = Some(PendingInteraction::McpForm(form));
                    Action::None
                }
            }
            PendingInteraction::McpUrl {
                id,
                server_name,
                message,
                url,
            } => match key.code {
                KeyCode::Char('o') => {
                    let target = url.clone();
                    self.pending = Some(PendingInteraction::McpUrl {
                        id,
                        server_name,
                        message,
                        url,
                    });
                    Action::OpenUrl(target)
                }
                KeyCode::Char('y') | KeyCode::Enter => Action::RpcResponse {
                    id,
                    result: mcp_elicitation_response("accept", None),
                },
                KeyCode::Char('n') => Action::RpcResponse {
                    id,
                    result: mcp_elicitation_response("decline", None),
                },
                KeyCode::Esc => Action::RpcResponse {
                    id,
                    result: mcp_elicitation_response("cancel", None),
                },
                _ => {
                    self.pending = Some(PendingInteraction::McpUrl {
                        id,
                        server_name,
                        message,
                        url,
                    });
                    Action::None
                }
            },
            PendingInteraction::Confirm {
                title,
                detail,
                action,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => action.into_action(),
                KeyCode::Char('n') | KeyCode::Esc => Action::None,
                _ => {
                    self.pending = Some(PendingInteraction::Confirm {
                        title,
                        detail,
                        action,
                    });
                    Action::None
                }
            },
            PendingInteraction::LoginMethodPicker { selected } => {
                let last = LoginMethod::CHOICES.len() - 1;
                match key.code {
                    KeyCode::Up | KeyCode::Left => {
                        self.pending = Some(PendingInteraction::LoginMethodPicker {
                            selected: selected.saturating_sub(1),
                        });
                        Action::None
                    }
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        self.pending = Some(PendingInteraction::LoginMethodPicker {
                            selected: (selected + 1).min(last),
                        });
                        Action::None
                    }
                    KeyCode::Char(digit @ '1'..='9') => {
                        let index = digit as usize - '1' as usize;
                        match LoginMethod::CHOICES.get(index) {
                            Some(method) => Action::StartLogin(*method),
                            None => {
                                self.pending =
                                    Some(PendingInteraction::LoginMethodPicker { selected });
                                Action::None
                            }
                        }
                    }
                    KeyCode::Enter => Action::StartLogin(LoginMethod::CHOICES[selected.min(last)]),
                    KeyCode::Esc => Action::None,
                    _ => {
                        self.pending = Some(PendingInteraction::LoginMethodPicker { selected });
                        Action::None
                    }
                }
            }
            PendingInteraction::Login {
                login_id,
                waiting_on,
            } => match key.code {
                KeyCode::Esc => Action::CancelLogin(login_id),
                // Everything else keeps waiting; only the server ends this state.
                _ => {
                    self.pending = Some(PendingInteraction::Login {
                        login_id,
                        waiting_on,
                    });
                    Action::None
                }
            },
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
            PendingInteraction::ThemePicker { theme_index } => Some(OverlayView {
                title: "Select theme".to_owned(),
                lines: ThemeKind::ALL
                    .iter()
                    .enumerate()
                    .map(|(index, candidate)| OverlayLine {
                        text: format!(
                            "{}. {}  ·  {}",
                            index + 1,
                            candidate.display_name(),
                            candidate.description()
                        ),
                        selected: index == *theme_index,
                        muted: false,
                    })
                    .collect(),
                hint: "1-3 select   ↑↓ navigate   Enter apply   Esc cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
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
            PendingInteraction::McpApproval(approval) => {
                let mut lines = vec![
                    OverlayLine {
                        text: approval.message.clone(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: approval.server_name.clone(),
                        selected: false,
                        muted: true,
                    },
                ];
                lines.extend(approval.detail.iter().map(|detail| OverlayLine {
                    text: detail.clone(),
                    selected: false,
                    muted: true,
                }));
                lines.extend(approval.options.iter().enumerate().map(|(index, option)| {
                    OverlayLine {
                        text: format!("{}. {} — {}", index + 1, option.label, option.description),
                        selected: index == approval.selected,
                        muted: false,
                    }
                }));
                Some(OverlayView {
                    title: "MCP approval".to_owned(),
                    lines,
                    hint: "↑↓ select   Enter confirm   Esc cancel".to_owned(),
                    style: OverlayStyle::Panel,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::McpForm(form) => {
                let field = form.fields.get(form.current)?;
                let mut lines = vec![
                    OverlayLine {
                        text: form.message.clone(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: format!(
                            "{} · {}/{}{}",
                            form.server_name,
                            form.current + 1,
                            form.fields.len(),
                            if field.required { " · required" } else { "" }
                        ),
                        selected: false,
                        muted: true,
                    },
                ];
                if !field.description.is_empty() {
                    lines.push(OverlayLine {
                        text: field.description.clone(),
                        selected: false,
                        muted: true,
                    });
                }
                let text_input = matches!(
                    &field.kind,
                    McpFieldKind::Text { .. } | McpFieldKind::Number { .. }
                );
                match &field.kind {
                    McpFieldKind::Boolean => {
                        let labels = if field.required {
                            vec!["No", "Yes"]
                        } else {
                            vec!["Not set", "No", "Yes"]
                        };
                        lines.extend(labels.into_iter().enumerate().map(|(index, label)| {
                            OverlayLine {
                                text: label.to_owned(),
                                selected: index == form.selected,
                                muted: false,
                            }
                        }));
                    }
                    McpFieldKind::SingleSelect(options) => {
                        if !field.required {
                            lines.push(OverlayLine {
                                text: "Not set".to_owned(),
                                selected: form.selected == 0,
                                muted: true,
                            });
                        }
                        let offset = usize::from(!field.required);
                        lines.extend(options.iter().enumerate().map(|(index, option)| {
                            OverlayLine {
                                text: option.label.clone(),
                                selected: index + offset == form.selected,
                                muted: false,
                            }
                        }));
                    }
                    McpFieldKind::MultiSelect { options, .. } => {
                        lines.extend(options.iter().enumerate().map(|(index, option)| {
                            OverlayLine {
                                text: format!(
                                    "[{}] {}",
                                    if form.checked.get(index) == Some(&true) {
                                        "x"
                                    } else {
                                        " "
                                    },
                                    option.label
                                ),
                                selected: index == form.selected,
                                muted: false,
                            }
                        }));
                    }
                    McpFieldKind::Text { .. } | McpFieldKind::Number { .. } => {}
                }
                if let Some(error) = form.validation_error.as_ref() {
                    lines.push(OverlayLine {
                        text: format!("! {error}"),
                        selected: false,
                        muted: false,
                    });
                }
                Some(OverlayView {
                    title: field.title.clone(),
                    lines,
                    hint: if text_input {
                        "Enter next   Alt+D decline   Esc cancel".to_owned()
                    } else if matches!(&field.kind, McpFieldKind::MultiSelect { .. }) {
                        "↑↓ move   Space toggle   Enter next   Alt+D decline   Esc cancel"
                            .to_owned()
                    } else {
                        "↑↓ select   Enter next   Alt+D decline   Esc cancel".to_owned()
                    },
                    style: OverlayStyle::Panel,
                    input: text_input.then_some(&form.editor),
                    input_label: "Value",
                    input_placeholder: "",
                })
            }
            PendingInteraction::McpUrl {
                server_name,
                message,
                url,
                ..
            } => Some(OverlayView {
                title: format!("{server_name} · Continue in browser"),
                lines: vec![
                    OverlayLine {
                        text: message.clone(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: url.clone(),
                        selected: false,
                        muted: true,
                    },
                    OverlayLine {
                        text: "[o] 브라우저 열기".to_owned(),
                        selected: true,
                        muted: false,
                    },
                    OverlayLine {
                        text: "[Enter] 완료 후 계속".to_owned(),
                        selected: false,
                        muted: false,
                    },
                    OverlayLine {
                        text: "[n] 거부".to_owned(),
                        selected: false,
                        muted: false,
                    },
                ],
                hint: "o open   Enter continue   n decline   Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::LoginMethodPicker { selected } => Some(OverlayView {
                title: "Select login method".to_owned(),
                lines: LoginMethod::CHOICES
                    .iter()
                    .enumerate()
                    .map(|(index, method)| OverlayLine {
                        text: format!("{}. {}  ·  {}", index + 1, method.label(), method.detail()),
                        selected: index == *selected,
                        muted: index != *selected,
                    })
                    .collect(),
                hint: "↑↓ select   Enter confirm   Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::Login { waiting_on, .. } => Some(OverlayView {
                title: "Signing in".to_owned(),
                lines: waiting_on
                    .iter()
                    .enumerate()
                    .map(|(index, text)| OverlayLine {
                        text: text.clone(),
                        selected: index == 0,
                        muted: index != 0,
                    })
                    .chain(std::iter::once(OverlayLine {
                        text: "완료되면 이 창이 자동으로 닫힙니다.".to_owned(),
                        selected: false,
                        muted: true,
                    }))
                    .collect(),
                hint: "Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::Confirm { title, detail, .. } => {
                let mut lines = detail
                    .iter()
                    .map(|text| OverlayLine {
                        text: text.clone(),
                        selected: false,
                        muted: false,
                    })
                    .collect::<Vec<_>>();
                lines.push(OverlayLine {
                    text: "[y] 계속".to_owned(),
                    selected: true,
                    muted: false,
                });
                lines.push(OverlayLine {
                    text: "[n] 취소".to_owned(),
                    selected: false,
                    muted: false,
                });
                Some(OverlayView {
                    title: title.clone(),
                    lines,
                    hint: "y / n".to_owned(),
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
            "{} Working… {} · Esc to interrupt",
            SPINNER[self.spinner_frame],
            format_elapsed(elapsed)
        ))
    }

    fn status_line(&self) -> StatusLineView {
        let context = self.context_window.and_then(|window| {
            (window > 0).then(|| {
                format!(
                    "ctx: {}/{} ({}%)",
                    format_token_count(self.context_tokens),
                    format_token_count(window),
                    // A prompt cannot really outgrow its window, but a stale
                    // reading should not print an impossible percentage.
                    (self.context_tokens.saturating_mul(100) / window).min(100)
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

    fn apply_theme(&mut self, selected: ThemeKind) -> Action {
        self.commit_welcome_card();
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            "✓ Theme changed",
            format!("↳ {}", selected.display_name()),
        ));
        Action::SetTheme(selected)
    }

    fn commit_welcome_card(&mut self) {
        if !self.show_welcome {
            return;
        }
        let welcome = self.welcome_view();
        self.committed.push(Block::welcome(
            &welcome.plan,
            &welcome.cwd,
            &welcome.account,
            &welcome.credits,
        ));
        self.show_welcome = false;
    }

    fn welcome_view(&self) -> WelcomeView {
        WelcomeView {
            plan: self.account_plan.plan_display(),
            credits: self.account_plan.credit_lines(),
            cwd: self.cwd.clone(),
            account: self.account.clone(),
        }
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
                // The renderer shows only the first few rows and counts the rest,
                // so this cap is a memory guard: keep it high enough that the
                // count it reports is the real one for any ordinary command.
                collapse_output(
                    item.get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    400,
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

/// Wall-clock elapsed for the activity row: `42s`, `1m 10s`, `1h 3m 49s`. A long
/// turn reads as minutes rather than as a three-digit second count.
fn format_elapsed(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds}s"),
        _ => format!("{hours}h {minutes}m {seconds}s"),
    }
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

    /// Shape captured from a real `account/rateLimits/read` response.
    fn rate_limits_fixture() -> Value {
        json!({
            "rateLimits": {
                "limitId": "codex",
                "planType": "prolite",
                "primary": { "usedPercent": 76, "windowDurationMins": 10080, "resetsAt": 1785276047 },
                "secondary": null,
                "credits": { "hasCredits": false, "unlimited": false, "balance": "0" }
            },
            "rateLimitResetCredits": {
                "availableCount": 3,
                "credits": [
                    { "id": "a", "resetType": "codexRateLimits", "status": "available",
                      "grantedAt": 1782932634, "expiresAt": 1785524634, "title": "Full reset" },
                    { "id": "b", "resetType": "codexRateLimits", "status": "available",
                      "grantedAt": 1783890530, "expiresAt": 1786482530, "title": "Full reset" },
                    { "id": "c", "resetType": "codexRateLimits", "status": "available",
                      "grantedAt": 1783964213, "expiresAt": 1786556213, "title": "Full reset" }
                ]
            }
        })
    }

    #[test]
    fn account_plan_reads_the_plan_and_reset_credits() {
        let plan = AccountPlan::from_rate_limits(&rate_limits_fixture());

        assert_eq!(plan.plan.as_deref(), Some("Pro 5x"));
        assert_eq!(plan.available_credits, 3);
        assert_eq!(plan.plan_display(), "Pro 5x");
        // Soonest expiry first, regardless of the order the server sent them.
        assert_eq!(
            plan.credits
                .iter()
                .map(|credit| credit.expires_at)
                .collect::<Vec<_>>(),
            [Some(1785524634), Some(1786482530), Some(1786556213)]
        );

        let lines = plan.credit_lines_at(1785524634 - 12 * 86_400);

        assert_eq!(lines[0], "3 available");
        assert_eq!(lines.len(), 4, "one summary row plus one row per credit");
        // Dates render in local time, so assert the shape and the relative span.
        assert!(
            lines[1].starts_with("· 20") && lines[1].ends_with("  12d left"),
            "unexpected credit row: {}",
            lines[1]
        );
    }

    #[test]
    fn credit_rows_list_each_expiry_and_cap_long_lists() {
        let credits = (0..6)
            .map(|index| json!({ "status": "available", "expiresAt": 1_000 + index * 86_400 }))
            .collect::<Vec<_>>();
        let plan = AccountPlan::from_rate_limits(&json!({
            "rateLimitResetCredits": { "availableCount": 6, "credits": credits }
        }));

        let lines = plan.credit_lines_at(0);

        assert_eq!(lines[0], "6 available");
        // Summary + CREDIT_LIST_LIMIT rows + the overflow note.
        assert_eq!(lines.len(), 1 + CREDIT_LIST_LIMIT + 1);
        assert_eq!(lines.last().map(String::as_str), Some("· +2 more"));
    }

    #[test]
    fn credit_rows_fall_back_when_an_expiry_is_missing() {
        let plan = AccountPlan::from_rate_limits(&json!({
            "rateLimitResetCredits": {
                "credits": [{ "status": "available", "title": "Full reset" }]
            }
        }));

        assert_eq!(
            plan.credit_lines_at(0),
            [
                "1 available".to_owned(),
                "· Full reset  no expiry".to_owned()
            ]
        );
    }

    #[test]
    fn account_plan_labels_every_known_plan_type() {
        // Every variant of the app-server's PlanType enum, plus separator forms.
        for (raw, expected) in [
            ("free", "Free"),
            ("go", "Go"),
            ("plus", "Plus"),
            ("prolite", "Pro 5x"),
            ("pro_lite", "Pro 5x"),
            ("PRO-LITE", "Pro 5x"),
            ("pro", "Pro 20x"),
            ("PRO", "Pro 20x"),
            ("team", "Team"),
            ("self_serve_business_usage_based", "Business (usage-based)"),
            ("business", "Business"),
            ("enterprise_cbp_usage_based", "Enterprise (usage-based)"),
            ("enterprise", "Enterprise"),
            ("edu", "Edu"),
            // Unknown plans pass through rather than being hidden.
            ("startup", "Startup"),
        ] {
            assert_eq!(plan_label(raw), expected, "planType {raw}");
        }
    }

    #[test]
    fn account_plan_degrades_when_the_server_reports_nothing() {
        let plan = AccountPlan::from_rate_limits(&json!({}));

        assert_eq!(plan, AccountPlan::default());
        assert_eq!(plan.plan_display(), "—");
        assert_eq!(plan.credit_lines(), ["none available".to_owned()]);
    }

    #[test]
    fn account_plan_ignores_spent_credits_and_flags_expiry() {
        let plan = AccountPlan::from_rate_limits(&json!({
            "rateLimitResetCredits": {
                "credits": [
                    { "status": "consumed", "expiresAt": 100 },
                    { "status": "available", "expiresAt": 500 }
                ]
            }
        }));

        assert_eq!(plan.available_credits, 1);
        assert_eq!(
            plan.credits
                .iter()
                .map(|c| c.expires_at)
                .collect::<Vec<_>>(),
            [Some(500)]
        );
        assert!(plan.credit_lines_at(600)[1].ends_with("  expired"));
    }

    #[test]
    fn welcome_card_carries_the_plan_instead_of_the_model() {
        let mut state = test_state();
        state.set_account_plan(AccountPlan::from_rate_limits(&rate_limits_fixture()));

        let welcome = state.welcome_view();

        assert_eq!(welcome.plan, "Pro 5x");
        assert_eq!(welcome.credits[0], "3 available");
        assert_eq!(welcome.credits.len(), 4);
        assert_eq!(welcome.cwd, state.cwd);
    }

    #[test]
    fn clearing_the_screen_brings_the_welcome_panel_back() {
        let mut state = test_state();
        // A first submit commits the welcome card and hides the live panel.
        state.editor.insert_str("hello");
        state.submit_editor();
        assert!(state.view().welcome.is_none());

        assert!(matches!(
            state.run_slash_command("/clear"),
            Action::ClearScreen
        ));
        state.reset_welcome();

        assert!(state.view().welcome.is_some());
    }

    #[test]
    fn ctrl_l_reaches_the_same_clear_action_as_the_command() {
        let mut state = test_state();

        let action = state.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));

        assert!(matches!(action, Action::ClearScreen));
    }

    #[test]
    fn login_puts_the_url_in_scrollback_and_waits_in_a_modal() {
        let mut state = test_state();
        let url = format!(
            "https://auth.openai.com/oauth/authorize?state={}",
            "x".repeat(300)
        );

        // /login only opens the method list; nothing is sent yet.
        assert!(matches!(state.run_slash_command("/login"), Action::None));

        state.begin_login("login-1".to_owned(), url.clone());

        assert_eq!(state.active_login_id(), Some("login-1"));
        // The URL is scrollback, not modal content, so the panel cannot be flooded.
        let block = state.committed.last().expect("sign-in url block");
        assert_eq!(block.title, "Sign-in URL");
        assert_eq!(block.body, url);
        let overlay = state.overlay_view().expect("login overlay");
        assert!(overlay.lines.iter().all(|line| !line.text.contains("http")));
    }

    #[test]
    fn login_method_list_selects_with_arrows_digits_or_enter() {
        let mut state = test_state();
        state.run_slash_command("/login");

        // Claude-Code-style list: numbered rows with the first one highlighted.
        let overlay = state.overlay_view().expect("login method list");
        assert_eq!(overlay.title, "Select login method");
        assert_eq!(overlay.lines.len(), LoginMethod::CHOICES.len());
        assert!(overlay.lines[0].text.starts_with("1. "));
        assert!(overlay.lines[0].selected);
        assert_eq!(overlay.hint, "↑↓ select   Enter confirm   Esc cancel");

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let confirmed = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            confirmed,
            Action::StartLogin(LoginMethod::DeviceCode)
        ));

        state.run_slash_command("/login");
        let by_digit = state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(matches!(by_digit, Action::StartLogin(LoginMethod::Browser)));
    }

    #[test]
    fn login_method_list_can_be_dismissed() {
        let mut state = test_state();
        state.run_slash_command("/login");

        let action = state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(matches!(action, Action::None));
        assert!(state.overlay_view().is_none());
    }

    #[test]
    fn waiting_modal_only_offers_cancel() {
        let mut state = test_state();
        state.begin_login("login-1".to_owned(), "https://example.test/auth".to_owned());

        let overlay = state.overlay_view().expect("login overlay");
        assert_eq!(overlay.hint, "Esc cancel");

        // The removed affordances no longer fire; the modal just keeps waiting.
        for key in ['o', 'c', 'y'] {
            let action = state.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert!(matches!(action, Action::None), "'{key}' should be inert");
            assert_eq!(state.active_login_id(), Some("login-1"));
        }

        let cancelled = state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(cancelled, Action::CancelLogin(id) if id == "login-1"));
    }

    #[test]
    fn device_login_shows_the_code_and_keeps_the_url_in_scrollback() {
        let mut state = test_state();

        state.begin_device_login(
            "login-2".to_owned(),
            "https://auth.openai.com/device".to_owned(),
            "ABCD-1234".to_owned(),
        );

        let block = state.committed.last().expect("sign-in url block");
        assert!(block.body.contains("https://auth.openai.com/device"));
        assert!(block.body.contains("ABCD-1234"));
        let overlay = state.overlay_view().expect("login overlay");
        assert!(overlay.lines[0].text.contains("ABCD-1234"));
        assert!(overlay.lines.iter().all(|line| !line.text.contains("http")));
    }

    #[test]
    fn login_methods_map_to_app_server_param_types() {
        assert_eq!(LoginMethod::Browser.param_type(), "chatgpt");
        assert_eq!(LoginMethod::DeviceCode.param_type(), "chatgptDeviceCode");
    }

    #[test]
    fn login_completed_notification_closes_the_modal_and_queues_a_refresh() {
        let mut state = test_state();
        state.begin_login("login-1".to_owned(), "https://example.test".to_owned());

        state.handle_notification(
            "account/login/completed",
            &json!({
                "loginId": "login-1", "success": true, "error": null
            }),
        );

        assert_eq!(state.active_login_id(), None);
        assert!(state.take_account_refresh());
        // The flag is consumed, so the event loop refreshes exactly once.
        assert!(!state.take_account_refresh());
    }

    /// One-off events belong in the composer notice next to the copy message,
    /// not in the status line where they used to park until the next thread.
    #[test]
    fn one_off_events_land_in_the_composer_notice_and_expire() {
        for (method, params, expected) in [
            ("skills/changed", json!({}), "Skills refreshed"),
            ("app/list/updated", json!({ "apps": [] }), "Apps refreshed"),
            (
                "model/rerouted",
                json!({ "fromModel": "Sol", "toModel": "Luna" }),
                "Sol → Luna로 전환됨",
            ),
        ] {
            let mut state = test_state();

            state.handle_notification(method, &params);

            let view = state.view();
            assert_eq!(view.composer_notice.as_deref(), Some(expected), "{method}");
            assert_eq!(
                view.status_line.and_then(|status| status.notice),
                None,
                "{method} should not park on the status line"
            );

            state.composer_notice = state.composer_notice.take().map(|(notice, _)| {
                (
                    notice,
                    Instant::now() - std::time::Duration::from_millis(1_500),
                )
            });
            assert!(state.tick(), "{method} should redraw once it expires");
            assert_eq!(state.view().composer_notice, None, "{method}");
        }
    }

    #[test]
    fn failed_login_reports_the_server_error_and_skips_the_refresh() {
        let mut state = test_state();
        state.begin_login("login-1".to_owned(), "https://example.test".to_owned());

        state.handle_notification(
            "account/login/completed",
            &json!({
                "loginId": "login-1", "success": false, "error": "browser closed"
            }),
        );

        assert_eq!(state.active_login_id(), None);
        assert!(!state.take_account_refresh());
        assert!(
            state
                .committed
                .iter()
                .any(|block| block.body.contains("browser closed"))
        );
    }

    #[test]
    fn logout_asks_before_dropping_credentials() {
        let mut state = test_state();

        assert!(matches!(state.run_slash_command("/logout"), Action::None));

        // Signing out needs a browser round trip to undo, so it confirms first.
        let overlay = state.overlay_view().expect("logout confirmation");
        assert!(overlay.title.contains("로그아웃"));

        let confirmed = state.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(matches!(confirmed, Action::Logout));
    }

    #[test]
    fn declined_logout_leaves_the_account_untouched() {
        let mut state = test_state();
        let before = state.account.clone();
        state.run_slash_command("/logout");

        let declined = state.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(matches!(declined, Action::None));
        assert!(state.overlay_view().is_none());
        assert_eq!(state.account, before);
    }

    #[test]
    fn applying_logout_clears_the_cached_identity() {
        let mut state = test_state();
        state.set_account_plan(AccountPlan::from_rate_limits(&rate_limits_fixture()));

        state.apply_logout();

        assert_eq!(state.welcome_view().plan, "—");
        assert_eq!(state.welcome_view().credits, ["none available".to_owned()]);
        assert!(state.account.contains("signed out"));
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
    fn fast_mode_is_shown_only_by_the_composer_badge() {
        let mut state = test_state();

        state.set_fast_mode(true);

        assert!(state.fast_mode);
        assert!(state.composer_mode().fast_mode);
        assert!(state.committed.is_empty());
        assert!(state.transient_status.is_none());
    }

    #[test]
    fn composer_badge_carries_both_permission_mode_and_fast_tier() {
        let mut state = test_state();
        state.permission_mode = PermissionMode::ReadOnly;
        state.set_fast_mode(false);

        let badge = state.composer_mode();

        assert_eq!(badge.label, "Read Only");
        assert!(!badge.fast_mode);

        state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(state.composer_mode().label, "Default");
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
    fn theme_command_supports_picker_and_direct_selection() {
        let mut state = test_state();

        assert!(matches!(state.run_slash_command("/theme"), Action::None));
        let overlay = state.overlay_view().expect("theme picker");
        assert_eq!(overlay.lines.len(), 3);
        assert!(overlay.lines[0].text.contains("Minimal"));
        state.pending = None;

        assert!(matches!(
            state.run_slash_command("/theme soft"),
            Action::SetTheme(ThemeKind::Soft)
        ));
        let card = state.committed.last().expect("theme card");
        assert_eq!(card.title, "✓ Theme changed");
        assert_eq!(card.body, "↳ Soft");
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
    fn the_resume_picker_pads_inside_its_border_and_leaves_the_search_rule_bare() {
        let picker = SessionPicker::new(
            vec![SessionInfo {
                id: "current".to_owned(),
                name: Some("Current project".to_owned()),
                preview: String::new(),
                cwd: r"C:\work\current".to_owned(),
                updated_at: 2,
            }],
            r"C:\work\current".to_owned(),
            None,
        );

        let view = picker.overlay_view();

        // The padding rows come from the panel renderer, so the picker itself
        // contributes only the session.
        assert_eq!(view.lines.len(), 1);
        assert!(
            view.input_label.is_empty(),
            "the placeholder already names the field"
        );
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
        state.context_tokens = 42;
        state.context_window = Some(100);
        state.transient_status = Some("old status".to_owned());
        state.busy = true;
        state.turn_id = Some("old-turn".to_owned());
        state.turn_started_at = Some(Instant::now());

        state.prepare_new_thread();

        assert!(state.editor.is_empty());
        assert!(state.committed.is_empty());
        assert_eq!(state.context_tokens, 0);
        assert_eq!(state.context_window, None);
        assert_eq!(state.transient_status, None);
        assert!(!state.busy);
        assert_eq!(state.turn_id, None);
        assert!(state.turn_started_at.is_none());
        assert!(state.view().welcome.is_some());
    }

    #[test]
    fn side_conversation_starts_while_a_turn_is_running() {
        let mut state = busy_state_with_live_turn();

        let action = state.run_slash_command("/btw");

        assert!(matches!(action, Action::StartSide(None)));
        assert!(
            !state
                .committed
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Warning))
        );
    }

    #[test]
    fn returning_from_a_side_conversation_restores_the_live_parent_turn() {
        let mut state = busy_state_with_live_turn();

        state.enter_side_thread(
            "fork-thread".to_owned(),
            "cwd".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );
        assert!(!state.busy, "the fork starts idle");

        let parked = state.take_side_parent_turn();
        state.prepare_resume();
        state.set_thread(
            "main-thread".to_owned(),
            "cwd".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );
        state.restore_turn(parked);

        assert!(state.busy);
        assert_eq!(state.turn_id.as_deref(), Some("live-turn"));
        assert!(state.turn_started_at.is_some());
    }

    #[test]
    fn a_parent_turn_that_ends_during_the_side_conversation_is_not_restored() {
        let mut state = busy_state_with_live_turn();
        state.enter_side_thread(
            "fork-thread".to_owned(),
            "cwd".to_owned(),
            "gpt-5.6-sol",
            Some("high"),
        );

        state.handle_notification("turn/completed", &json!({ "threadId": "main-thread" }));

        assert!(state.take_side_parent_turn().is_none());
        assert!(!state.busy);
    }

    fn busy_state_with_live_turn() -> AppState {
        let model = test_model("gpt-5.6-sol", "GPT-5.6-Sol", true);
        let mut state = AppState::new(
            "main-thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "gpt-5.6-sol",
            Some("high"),
        );
        state.busy = true;
        state.turn_id = Some("live-turn".to_owned());
        state.turn_started_at = Some(Instant::now());
        state
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

    #[test]
    fn mcp_form_returns_typed_structured_content() {
        let params = json!({
            "serverName": "example",
            "message": "Configure the tool",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "required": ["a_name", "b_count", "d_mode", "e_tags"],
                "properties": {
                    "a_name": {
                        "type": "string",
                        "title": "Name",
                        "minLength": 2
                    },
                    "b_count": {
                        "type": "integer",
                        "default": 3,
                        "minimum": 1
                    },
                    "c_enabled": {
                        "type": "boolean"
                    },
                    "d_mode": {
                        "type": "string",
                        "enum": ["safe", "fast"],
                        "enumNames": ["Safe", "Fast"]
                    },
                    "e_tags": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["rust", "cli"]
                        },
                        "minItems": 1
                    }
                }
            }
        });
        let mut form = McpForm::parse(json!(7), &params).expect("valid form");

        form.editor.set_text("Devez");
        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert_eq!(form.editor.text(), "3");
        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(
            form.handle_key(KeyEvent::from(KeyCode::Char(' ')))
                .is_none()
        );
        let response = form
            .handle_key(KeyEvent::from(KeyCode::Enter))
            .expect("accepted response");

        assert_eq!(
            response.get("action").and_then(Value::as_str),
            Some("accept")
        );
        assert_eq!(
            response.pointer("/content/a_name").and_then(Value::as_str),
            Some("Devez")
        );
        assert_eq!(
            response.pointer("/content/b_count").and_then(Value::as_i64),
            Some(3)
        );
        assert!(response.pointer("/content/c_enabled").is_none());
        assert_eq!(
            response.pointer("/content/d_mode").and_then(Value::as_str),
            Some("safe")
        );
        assert_eq!(
            response
                .pointer("/content/e_tags/0")
                .and_then(Value::as_str),
            Some("rust")
        );
    }

    #[test]
    fn mcp_form_validates_required_fields_and_can_cancel() {
        let params = json!({
            "serverName": "example",
            "message": "Email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "required": ["email"],
                "properties": {
                    "email": {
                        "type": "string",
                        "format": "email"
                    }
                }
            }
        });
        let mut form = McpForm::parse(json!(1), &params).expect("valid form");

        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(form.validation_error.is_some());
        form.editor.set_text("invalid");
        assert!(form.handle_key(KeyEvent::from(KeyCode::Enter)).is_none());
        assert!(form.validation_error.is_some());
        let response = form
            .handle_key(KeyEvent::from(KeyCode::Esc))
            .expect("cancel response");
        assert_eq!(
            response.get("action").and_then(Value::as_str),
            Some("cancel")
        );
        assert!(response.get("content").is_some_and(Value::is_null));
    }

    #[test]
    fn unsupported_mcp_forms_decline_instead_of_breaking_the_turn() {
        let mut state = test_state();
        let action = state.begin_server_request(
            json!(9),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "custom",
                "message": "Unsupported",
                "mode": "openai/form",
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "file" }
                    }
                }
            }),
        );

        match action {
            Action::RpcResponse { result, .. } => {
                assert_eq!(
                    result.get("action").and_then(Value::as_str),
                    Some("decline")
                );
            }
            _ => panic!("unsupported form should be declined"),
        }
    }

    #[test]
    fn mcp_url_prompt_opens_without_losing_the_pending_reply() {
        let mut state = test_state();
        let action = state.begin_server_request(
            json!(4),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "github",
                "message": "Authorize",
                "mode": "url",
                "url": "https://example.com/auth",
                "elicitationId": "elicit-1"
            }),
        );
        assert!(matches!(action, Action::None));

        let open = state.handle_key(KeyEvent::from(KeyCode::Char('o')));
        assert!(matches!(open, Action::OpenUrl(ref url) if url == "https://example.com/auth"));
        let accept = state.handle_key(KeyEvent::from(KeyCode::Enter));
        match accept {
            Action::RpcResponse { result, .. } => {
                assert_eq!(result.get("action").and_then(Value::as_str), Some("accept"));
            }
            _ => panic!("URL prompt should accept after browser flow"),
        }
    }

    #[test]
    fn resolved_server_request_closes_only_the_matching_prompt() {
        let mut state = test_state();
        state.begin_server_request(
            json!(4),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "github",
                "message": "Authorize",
                "mode": "url",
                "url": "https://example.com/auth",
                "elicitationId": "elicit-1"
            }),
        );
        state.handle_notification(
            "serverRequest/resolved",
            &json!({ "threadId": state.thread_id.clone(), "requestId": 7 }),
        );
        assert!(matches!(
            state.pending,
            Some(PendingInteraction::McpUrl { .. })
        ));
        state.handle_notification(
            "serverRequest/resolved",
            &json!({ "threadId": state.thread_id.clone(), "requestId": 4 }),
        );
        assert!(state.pending.is_none());
    }

    #[test]
    fn mcp_tool_approval_returns_requested_persist_scope() {
        let mut state = test_state();
        let action = state.begin_server_request(
            json!(12),
            "mcpServer/elicitation/request",
            &json!({
                "serverName": "calendar",
                "message": "Create an event?",
                "mode": "form",
                "requestedSchema": {
                    "type": "object",
                    "properties": {}
                },
                "_meta": {
                    "codex_approval_kind": "mcp_tool_call",
                    "persist": ["session", "always"],
                    "tool_params_display": [{
                        "name": "title",
                        "display_name": "Title",
                        "value": "Roadmap"
                    }]
                }
            }),
        );
        assert!(matches!(action, Action::None));
        assert!(matches!(
            state.pending,
            Some(PendingInteraction::McpApproval(ref approval))
                if approval.options.len() == 4 && approval.detail == ["Title: Roadmap"]
        ));

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Down)),
            Action::None
        ));
        let response = state.handle_key(KeyEvent::from(KeyCode::Enter));
        match response {
            Action::RpcResponse { result, .. } => {
                assert_eq!(result.get("action").and_then(Value::as_str), Some("accept"));
                assert_eq!(
                    result.pointer("/_meta/persist").and_then(Value::as_str),
                    Some("session")
                );
            }
            _ => panic!("MCP approval should return a scoped accept response"),
        }
    }

    #[test]
    fn explicit_skill_plugin_and_app_mentions_become_typed_turn_items() {
        let mut state = test_state();
        state.update_skills(&json!({
            "data": [{
                "cwd": "cwd",
                "errors": [],
                "skills": [
                    {
                        "name": "review",
                        "path": "C:/skills/review/SKILL.md",
                        "description": "Review",
                        "enabled": true,
                        "scope": "user"
                    },
                    {
                        "name": "disabled",
                        "path": "C:/skills/disabled/SKILL.md",
                        "description": "Disabled",
                        "enabled": false,
                        "scope": "user"
                    }
                ]
            }]
        }));
        state.update_plugins(&json!({
            "marketplaces": [{
                "name": "openai-bundled",
                "plugins": [{
                    "id": "github@openai-bundled",
                    "name": "github",
                    "installed": true,
                    "enabled": true,
                    "interface": { "displayName": "GitHub Tools" }
                }]
            }]
        }));
        state.update_apps(&json!({
            "data": [{
                "id": "calendar",
                "name": "Calendar",
                "isAccessible": true,
                "isEnabled": true
            }]
        }));

        let input = state.turn_input(
            "$review check this with @github-tools and $calendar; ignore $disabled".to_owned(),
        );
        assert_eq!(input.len(), 4);
        assert_eq!(input[1].get("type").and_then(Value::as_str), Some("skill"));
        assert_eq!(
            input[1].get("path").and_then(Value::as_str),
            Some("C:/skills/review/SKILL.md")
        );
        assert_eq!(
            input[2].get("type").and_then(Value::as_str),
            Some("mention")
        );
        assert_eq!(
            input[2].get("path").and_then(Value::as_str),
            Some("plugin://github@openai-bundled")
        );
        assert_eq!(
            input[3].get("path").and_then(Value::as_str),
            Some("app://calendar")
        );
    }

    #[test]
    fn mention_tokens_do_not_treat_email_addresses_as_plugins() {
        assert_eq!(
            mention_triggers("mail foo@sample.com, then use @sample"),
            vec!["sample".to_owned()]
        );
    }

    #[test]
    fn integration_slash_commands_dispatch_app_server_actions() {
        let mut state = test_state();
        assert!(matches!(state.run_slash_command("/mcp"), Action::ShowMcp));
        assert!(matches!(
            state.run_slash_command("/mcp login github"),
            Action::McpLogin(ref name) if name == "github"
        ));
        assert!(matches!(
            state.run_slash_command("/plugins install browser"),
            Action::PreparePluginInstall(ref name) if name == "browser"
        ));
        assert!(matches!(
            state.run_slash_command("/plugins disable browser"),
            Action::SetPlugin {
                ref query,
                enabled: false
            } if query == "browser"
        ));
        assert!(matches!(
            state.run_slash_command("/skills disable imagegen"),
            Action::SetSkill {
                ref name,
                enabled: false
            } if name == "imagegen"
        ));
        assert!(matches!(
            state.run_slash_command("/apps enable calendar"),
            Action::SetApp {
                ref query,
                enabled: true
            } if query == "calendar"
        ));
    }

    /// The thread tally outgrows the window within a few turns, so reading it as
    /// context usage produced readings like `1103k/258k (426%)`.
    #[test]
    fn context_gauge_tracks_the_last_turn_not_the_thread_tally() {
        let mut state = test_state();
        state.handle_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": {
                    "total": { "totalTokens": 1_103_000 },
                    "last": { "totalTokens": 96_400 },
                    "modelContextWindow": 258_000
                }
            }),
        );

        assert_eq!(state.context_tokens, 96_400);
        assert_eq!(state.context_window, Some(258_000));
        assert_eq!(
            state.status_line().context.as_deref(),
            Some("ctx: 96k/258k (37%)")
        );
    }

    #[test]
    fn composer_badge_reports_an_estimated_cost_from_the_billed_totals() {
        let mut state = test_state();
        state.handle_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": {
                    "total": {
                        "totalTokens": 580_000,
                        "inputTokens": 570_000,
                        "cachedInputTokens": 500_000,
                        "cacheWriteInputTokens": 40_000,
                        "outputTokens": 10_000
                    },
                    "last": { "totalTokens": 96_400 },
                    "modelContextWindow": 258_000
                }
            }),
        );

        // gpt-5.6 at $5/$30 per million: 30k fresh input (0.15) + 40k cache
        // write ×1.25 (0.25) + 500k cache read ×0.1 (0.25) + 10k output (0.30).
        assert_eq!(state.composer_mode().cost.as_deref(), Some("$0.95"));
        assert_eq!(state.token_totals.input_new, 30_000);
    }

    #[test]
    fn long_turns_read_as_minutes_instead_of_raw_seconds() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(42), "42s");
        assert_eq!(format_elapsed(70), "1m 10s");
        assert_eq!(format_elapsed(229), "3m 49s");
        assert_eq!(format_elapsed(3_600), "1h 0m 0s");
        assert_eq!(format_elapsed(3_829), "1h 3m 49s");
    }
}
