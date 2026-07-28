use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use serde_json::{Map, Value, json};

use crate::{
    completion::{
        CompletionCandidate, CompletionKind, CompletionMode, CompletionTarget, completion_target,
        completion_text, filter_candidates,
    },
    editor::Editor,
    integrations::{
        MarketplacePicker, MarketplacePickerResult, McpPicker, McpPickerResult, McpServerInfo,
        PluginCatalog, PluginDetail, PluginInfo, PluginPicker, PluginPickerResult, PluginScope,
        PluginTarget,
    },
    pricing::{self, CostLedger, TokenTotals},
    renderer::{
        Block, BlockKind, ComposerMode, EffortSlider, HIDDEN_STATUS_LINE, ModeAccent, OverlayLine,
        OverlayStyle, OverlayView, PICKER_ROWS, PlanStep, PlanStepStatus, PlanSummary,
        StatusLineView, SuggestionView, VibeTone, View, WelcomeView,
        visible_window,
    },
    rollout::{PlanSnapshot, Rollout, RolloutEvent, RolloutKind},
    theme::{self, ThemeKind},
};

const SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

/// How long one shimmer sweep across the `Working` label takes.
const SHIMMER_PERIOD: Duration = Duration::from_millis(1_100);

/// The permission presets Codex exposes through `/permissions`, cycled with Shift+Tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionMode {
    FullAccess,
}

/// Controls the desired response length. Codex names the underlying setting
/// `model_verbosity`, so the mapping is intentionally inverted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResponseLength {
    #[default]
    Short,
    Normal,
    Detailed,
}

impl ResponseLength {
    pub fn label(self) -> &'static str {
        match self {
            Self::Short => "Short",
            Self::Normal => "Normal",
            Self::Detailed => "Detailed",
        }
    }

    pub fn model_verbosity(self) -> &'static str {
        match self {
            Self::Short => "low",
            Self::Normal => "medium",
            Self::Detailed => "high",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Short => Self::Normal,
            Self::Normal => Self::Detailed,
            Self::Detailed => Self::Short,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellDisplayMode {
    #[default]
    Hide,
    Collapse,
    Expand,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffDisplayMode {
    #[default]
    Hide,
    Collapse,
    Expand,
}

#[derive(Clone, Copy)]
enum StatusLineField {
    Model,
    Effort,
    Context,
    FiveHour,
    Weekly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VibeMode {
    #[default]
    Vibe,
    SuperVibe,
    Normal,
}

impl VibeMode {
    pub const fn config_value(self) -> &'static str { match self { Self::Vibe => "vibe", Self::SuperVibe => "super_vibe", Self::Normal => "normal" } }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Vibe => "Vibe: On",
            Self::SuperVibe => "Vibe: Super Vibe",
            Self::Normal => "Vibe: Off",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Vibe => Self::SuperVibe,
            Self::SuperVibe => Self::Normal,
            Self::Normal => Self::Vibe,
        }
    }
}

impl StatusLineField {
    const ALL: [Self; 5] = [
        Self::Model,
        Self::Effort,
        Self::Context,
        Self::FiveHour,
        Self::Weekly,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::Effort => "Effort",
            Self::Context => "Context",
            Self::FiveHour => "5h limit",
            Self::Weekly => "Weekly limit",
        }
    }

    const fn config_key(self) -> &'static str {
        match self {
            Self::Model => "status_line_model",
            Self::Effort => "status_line_effort",
            Self::Context => "status_line_context",
            Self::FiveHour => "status_line_five_hour",
            Self::Weekly => "status_line_weekly",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Model => 0,
            Self::Effort => 1,
            Self::Context => 2,
            Self::FiveHour => 3,
            Self::Weekly => 4,
        }
    }
}

#[derive(Clone, Copy)]
struct StatusLineSettings([bool; 5]);

impl Default for StatusLineSettings {
    fn default() -> Self {
        Self([true; 5])
    }
}

impl StatusLineSettings {
    const fn enabled(self, field: StatusLineField) -> bool {
        self.0[field.index()]
    }

    fn toggle(&mut self, field: StatusLineField) -> bool {
        let enabled = &mut self.0[field.index()];
        *enabled = !*enabled;
        *enabled
    }
}

impl DiffDisplayMode {
    #[allow(dead_code)]
    fn from_config_value(value: &str) -> Option<Self> {
        match value
            .trim()
            .trim_matches(['"', '\''])
            .to_ascii_lowercase()
            .as_str()
        {
            "hide" => Some(Self::Hide),
            "collapse" => Some(Self::Collapse),
            "expand" => Some(Self::Expand),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Hide => "Hide",
            Self::Collapse => "Collapse",
            Self::Expand => "Expand",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Hide => Self::Collapse,
            Self::Collapse => Self::Expand,
            Self::Expand => Self::Hide,
        }
    }

    pub const fn config_value(self) -> &'static str {
        match self {
            Self::Hide => "hide",
            Self::Collapse => "collapse",
            Self::Expand => "expand",
        }
    }
}

impl ShellDisplayMode {
    #[allow(dead_code)]
    fn from_config_value(value: &str) -> Option<Self> {
        match value
            .trim()
            .trim_matches(['"', '\''])
            .to_ascii_lowercase()
            .as_str()
        {
            "hide" => Some(Self::Hide),
            "collapse" => Some(Self::Collapse),
            "expand" => Some(Self::Expand),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hide => "Hide",
            Self::Collapse => "Collapse",
            Self::Expand => "Expand",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Hide => Self::Collapse,
            Self::Collapse => Self::Expand,
            Self::Expand => Self::Hide,
        }
    }

    pub const fn config_value(self) -> &'static str {
        match self {
            Self::Hide => "hide",
            Self::Collapse => "collapse",
            Self::Expand => "expand",
        }
    }
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::FullAccess => "Full Access",
        }
    }

    /// Built-in permission profile id understood by the app-server.
    pub fn profile(self) -> &'static str {
        match self {
            Self::FullAccess => ":danger-full-access",
        }
    }

    fn accent(self) -> ModeAccent {
        match self {
            Self::FullAccess => ModeAccent::Danger,
        }
    }

}

struct SlashCommand {
    name: &'static str,
    description: &'static str,
    takes_argument: bool,
}

const SLASH_COMMANDS: [SlashCommand; 27] = [
    SlashCommand {
        name: "/model",
        description: "Switch model and reasoning",
        takes_argument: true,
    },
    SlashCommand {
        name: "/fast",
        description: "Choose the model's fast service tier",
        takes_argument: true,
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
        description: "Browse MCP servers, reconnect, or sign in",
        takes_argument: true,
    },
    SlashCommand {
        name: "/plugins",
        description: "Browse plugins and manage marketplaces",
        takes_argument: true,
    },
    SlashCommand {
        name: "/skills",
        description: "List, enable, or disable Codex skills",
        takes_argument: true,
    },
    SlashCommand {
        name: "/reload-plugins",
        description: "Apply plugin changes to this session",
        takes_argument: false,
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
        description: "Choose how file changes are displayed",
        takes_argument: true,
    },
    SlashCommand {
        name: "/shell",
        description: "Choose how shell commands are displayed",
        takes_argument: true,
    },
    SlashCommand {
        name: "/vibemode",
        description: "Customize response, shell, and diff display",
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
        name: "/statusline",
        description: "Show or hide the status line",
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
        description: "Exit Devez Vibe",
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
    ScrollToBottom,
    Copy(String),
    #[allow(dead_code)]
    ShowDiff,
    /// Fetch MCP server status and open the picker. Any notice is carried over
    /// so the result of the action that reopened it stays on screen.
    OpenMcp(Option<String>),
    McpLogin(String),
    /// Re-read the MCP configuration and restart the servers.
    ReconnectMcp,
    StartLogin(LoginMethod),
    CancelLogin(String),
    Logout,
    OpenPlugins {
        scope: Option<PluginScope>,
        notice: Option<String>,
    },
    /// Read one plugin's contents, then reopen the picker on its detail page.
    OpenPluginDetail {
        target: PluginTarget,
        origin: Option<PluginScope>,
    },
    PreparePluginInstall(String),
    PreparePluginUninstall(String),
    SetPlugin {
        query: String,
        enabled: bool,
    },
    /// Toggle a plugin the picker already resolved, so no re-lookup is needed.
    SetPluginEnabled {
        plugin: Box<PluginInfo>,
        enabled: bool,
    },
    ConfirmPluginInstall(Box<PluginInfo>),
    ConfirmPluginUninstall(Box<PluginInfo>),
    InstallPlugin(PluginInstallTarget),
    UninstallPlugin(PluginUninstallTarget),
    OpenMarketplaces(Option<String>),
    ConfirmMarketplaceAdd(String),
    AddMarketplace(String),
    ConfirmMarketplaceRemove(String),
    RemoveMarketplace(String),
    /// Refreshes every configured git marketplace; see
    /// `MarketplacePickerResult::UpgradeAll` for why it is not per-marketplace.
    UpgradeMarketplaces,
    /// Re-reads skills, plugins and apps and restarts the MCP servers, so a
    /// plugin installed this session takes effect without relaunching.
    ReloadPlugins,
    ShowSkills,
    SetSkill {
        name: String,
        enabled: bool,
    },
    RefreshSkills,
    OpenUrl(String),
    SetTheme(ThemeKind),
    /// Write the picked model and effort into `~/.codex/config.toml`.
    PersistModelDefault {
        model: String,
        effort: String,
    },
    /// Save the transcript's Shell display preference for future sessions.
    PersistShellDisplayMode(ShellDisplayMode),
    PersistDiffDisplayMode(DiffDisplayMode),
    PersistVibeDisplayModes {
        vibe: VibeMode,
        response: ResponseLength,
        shell: ShellDisplayMode,
        diff: DiffDisplayMode,
    },
    PersistConversationView(ConversationView),
    PersistStatusLine {
        key_path: &'static str,
        enabled: bool,
    },
    Quit,
    ClearScreen,
    Tick(bool),
    RpcResponse {
        id: Value,
        result: Value,
    },
    RpcError {
        id: Value,
        message: String,
    },
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
    AddMarketplace(String),
    RemoveMarketplace(String),
    Logout,
}

impl ConfirmedAction {
    fn into_action(self) -> Action {
        match self {
            Self::InstallPlugin(target) => Action::InstallPlugin(target),
            Self::UninstallPlugin(target) => Action::UninstallPlugin(target),
            Self::AddMarketplace(source) => Action::AddMarketplace(source),
            Self::RemoveMarketplace(name) => Action::RemoveMarketplace(name),
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
    shell_batch: Option<String>,
}

struct ShellBatch {
    anchor: Block,
    members: Vec<String>,
    completed: HashMap<String, ShellResult>,
}

#[derive(Clone)]
struct ShellResult {
    block: Block,
    exit_code: Option<i64>,
    duration_ms: Option<u64>,
}

/// How long a model pick should last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelScope {
    Session,
    Default,
}

impl ModelScope {
    const CHOICES: [Self; 2] = [Self::Session, Self::Default];

    fn label(self) -> &'static str {
        match self {
            Self::Session => "This session only",
            Self::Default => "Set as default",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Session => "Returns to the original setting next time",
            Self::Default => "Saves to ~/.codex/config.toml",
        }
    }
}

enum PendingInteraction {
    ModelPicker {
        model_index: usize,
        effort_index: usize,
    },
    EffortPicker {
        effort_index: usize,
    },
    SettingPicker {
        setting: DisplaySetting,
        selected: usize,
    },
    VibeModePicker {
        row: usize,
        vibe: VibeMode,
        response: ResponseLength,
        shell: ShellDisplayMode,
        diff: DiffDisplayMode,
    },
    StatusLinePicker {
        selected: usize,
    },
    /// Second step of `/model`: how long the pick should last. Asked after the
    /// model is chosen so the common case (this session) stays two keystrokes.
    ModelScope {
        model_index: usize,
        effort_index: usize,
        selected: usize,
    },
    ThemePicker {
        theme_index: usize,
    },
    SessionPicker(SessionPicker),
    McpPicker(McpPicker),
    PluginPicker(PluginPicker),
    /// Reached from the plugin picker, so cancelling it returns there instead of
    /// closing the overlay outright.
    MarketplacePicker(MarketplacePicker),
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
    /// Numbered list that picks the sign-in flow before starting it.
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

#[derive(Clone, Copy)]
enum DisplaySetting {
    Shell,
    Diff,
    Fast,
}

impl DisplaySetting {
    fn title(self) -> &'static str {
        match self {
            Self::Shell => "Shell",
            Self::Diff => "Diff",
            Self::Fast => "Fast",
        }
    }

    fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Shell | Self::Diff => &["Hide", "Collapse", "Expand"],
            Self::Fast => &["On", "Off"],
        }
    }
}

#[derive(Clone)]
struct SkillBinding {
    name: String,
    path: String,
    description: String,
    enabled: bool,
}

#[derive(Clone)]
struct MentionBinding {
    trigger: String,
    name: String,
    path: String,
    description: String,
}

struct SelectedCompletionBinding {
    sigil: char,
    trigger: String,
    token: String,
    range: Range<usize>,
    kind: CompletionKind,
    name: String,
    path: String,
}

impl SelectedCompletionBinding {
    fn matches_text(&self, chars: &[char]) -> bool {
        chars
            .get(self.range.clone())
            .is_some_and(|value| value.iter().copied().eq(self.token.chars()))
            && self.range.start.checked_sub(1).is_none_or(|index| {
                chars
                    .get(index)
                    .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
            })
            && chars.get(self.range.end).is_none_or(|ch| {
                !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
            })
    }

    fn typed_item(&self) -> Value {
        match self.kind {
            CompletionKind::Skill => json!({
                "type": "skill",
                "name": self.name,
                "path": self.path
            }),
            CompletionKind::Plugin | CompletionKind::App => json!({
                "type": "mention",
                "name": self.name,
                "path": self.path
            }),
            CompletionKind::File | CompletionKind::Directory => {
                unreachable!("filesystem completions do not carry typed bindings")
            }
        }
    }
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
                description: skill
                    .get("description")
                    .and_then(Value::as_str)
                    .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned(),
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
            mentions.extend(triggers.into_iter().map(|trigger| {
                MentionBinding {
                    trigger,
                    name: display_name.to_owned(),
                    path: path.clone(),
                    description: plugin
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("Plugin")
                        .to_owned(),
                }
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
        mentions.extend(triggers.into_iter().map(|trigger| {
            MentionBinding {
                trigger,
                name: name.to_owned(),
                path: path.clone(),
                description: app
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("App")
                    .to_owned(),
            }
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

fn mention_triggers(text: &str) -> Vec<(char, String)> {
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
            triggers.push((*ch, text[start..end].to_owned()));
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

const RESUME_PICKER_ROWS: usize = 10;
/// Compact resume picker chrome: panel, search input, and status rows.
const RESUME_PICKER_CHROME_ROWS: u16 = 11;

/// Session rows that fit beneath the resume picker's fixed chrome. Keeping this
/// dynamic prevents `fit_frame` from dropping the panel header on short grids.
fn resume_picker_rows(height: u16) -> usize {
    usize::from(
        height
            .saturating_sub(RESUME_PICKER_CHROME_ROWS)
            .clamp(1, RESUME_PICKER_ROWS as u16),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConversationView { #[default] List, Chat }

impl ConversationView {
    pub const fn label(self) -> &'static str { match self { Self::List => "List", Self::Chat => "Chat" } }
    pub const fn config_value(self) -> &'static str { match self { Self::List => "list", Self::Chat => "chat" } }
    pub const fn next(self) -> Self { match self { Self::List => Self::Chat, Self::Chat => Self::List } }
    pub const fn is_chat(self) -> bool { matches!(self, Self::Chat) }
}

fn visible_resume_picker_rows() -> usize {
    let height = terminal::size().map(|(_, height)| height).unwrap_or(30);
    resume_picker_rows(height)
}

/// Rows the `Apply to` step prints before its choices: the pick it is confirming
/// and the blank under it.
const MODEL_SCOPE_HEADER_ROWS: usize = 2;

/// Whether a panel wears the `✕` that shuts it. The user opened these three and
/// the session list themselves, so they may drop them; everything else on this
/// enum is a question owed an answer — an approval, a login, a server prompt —
/// and shutting it would leave the session waiting on nothing. The panels that
/// paint the mark set `OverlayView::closable`, and this is what a click on it
/// checks, so a mark that appears is a mark that works.
fn closable_overlay(pending: &PendingInteraction) -> bool {
    matches!(
        pending,
        PendingInteraction::ModelPicker { .. }
            | PendingInteraction::ModelScope { .. }
            | PendingInteraction::EffortPicker { .. }
            | PendingInteraction::SettingPicker { .. }
            | PendingInteraction::VibeModePicker { .. }
            | PendingInteraction::StatusLinePicker { .. }
            | PendingInteraction::SessionPicker(_)
    )
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

    /// A click on one of the painted rows. `row` counts from the top of the
    /// visible window, so it is resolved against the same window the rows were
    /// painted from; one click resumes, exactly as Enter on that row would.
    pub fn click_row(&mut self, row: usize) -> SessionPickerResult {
        let rows = visible_resume_picker_rows();
        if row >= rows {
            return SessionPickerResult::None;
        }
        let filtered = self.filtered();
        let start = visible_window(Some(self.selected), filtered.len(), rows).start;
        let Some(session) = filtered.get(start + row) else {
            // The "no sessions" placeholder, which stands for nothing to resume.
            return SessionPickerResult::None;
        };
        let id = session.id.clone();
        self.selected = start + row;
        SessionPickerResult::Select(id)
    }

    pub fn overlay_view(&self) -> OverlayView<'_> {
        let filtered = self.filtered();
        let window = visible_window(
            Some(self.selected),
            filtered.len(),
            visible_resume_picker_rows(),
        );
        let start = window.start;
        let mut lines = filtered[window]
            .iter()
            .enumerate()
            .map(|(offset, session)| {
                let index = start + offset;
                let current = self
                    .current_thread_id
                    .as_deref()
                    .is_some_and(|id| id == session.id);
                let folder = if self.all_projects {
                    format!("  {}", session.cwd)
                } else {
                    String::new()
                };
                OverlayLine {
                    text: format!(
                        "{:<8}  {}{}{}",
                        relative_time(session.updated_at),
                        session.title(),
                        if current { "  ·  current" } else { "" },
                        folder
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
            closable: true,
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
            slider: None,
            hint: "↑↓ navigate  Enter resume  Ctrl+A all projects  Esc cancel".to_owned(),
            style: OverlayStyle::CompactPanel,
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
    composer_images: Vec<String>,
    queued_prompts: VecDeque<String>,
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// Set when the user interrupts after `turn/start` answers but before the
    /// app-server has announced that the turn is active.
    pending_interrupt: bool,
    turn_interrupted: bool,
    quit_armed: bool,
    pub busy: bool,
    pub cwd: String,
    account: String,
    models: Vec<ModelInfo>,
    selected_model: usize,
    selected_effort: String,
    committed: Vec<Block>,
    active_order: Vec<String>,
    active: HashMap<String, ActiveItem>,
    shell_batches: HashMap<String, ShellBatch>,
    /// Completed Shell calls in the current turn. Sequential batches keep
    /// updating one transcript row instead of leaving one row per command.
    turn_shell_results: Vec<ShellResult>,
    turn_shell_anchor: Option<Block>,
    turn_shell_duration_ms: Option<u64>,
    /// Completed file changes in the current turn. Later changes replace the
    /// first transcript row instead of appending another collapsed card.
    turn_file_changes: Vec<Block>,
    turn_file_change_anchor: Option<Block>,
    /// App-server lifecycle notifications can be replayed. An item id belongs
    /// to one logical operation, so only its first completion may reach history.
    completed_item_ids: HashSet<String>,
    /// Exact operation cards already emitted in the current turn. This keeps
    /// repeated searches/tool results from producing identical transcript rows.
    seen_operation_signatures: HashSet<String>,
    pending: Option<PendingInteraction>,
    /// Tokens the *current* prompt occupies, not the thread's running tally.
    /// The tally climbs past the window on every turn and is not a context gauge.
    context_tokens: u64,
    /// The running tally, which is what billing counts.
    token_totals: TokenTotals,
    cost_ledger: Option<CostLedger>,
    pending_turn_model: Option<String>,
    pending_turn_effort: Option<String>,
    active_turn_model: Option<String>,
    active_turn_effort: Option<String>,
    cost_restore_due: bool,
    cost_restore_pending: bool,
    context_window: Option<u64>,
    transient_status: Option<String>,
    show_welcome: bool,
    welcome_credits_expanded: bool,
    plan_summary: Option<PlanSummary>,
    command_selection: usize,
    spinner_frame: usize,
    turn_started_at: Option<Instant>,
    last_completed_duration: Option<Duration>,
    branch: Option<String>,
    five_hour_percent: Option<u8>,
    weekly_percent: Option<u8>,
    fast_mode: bool,
    side_parent: Option<SideParent>,
    last_assistant_markdown: Option<String>,
    composer_notice: Option<(String, Instant)>,
    activity_notice: Option<(String, Instant)>,
    status_metadata_refreshed_at: Instant,
    response_length: ResponseLength,
    vibe_mode: VibeMode,
    conversation_view: ConversationView,
    shell_display_mode: ShellDisplayMode,
    diff_display_mode: DiffDisplayMode,
    status_line_settings: StatusLineSettings,
    account_plan: AccountPlan,
    /// Set when a login lands, so the event loop re-reads the account over RPC.
    account_refresh_due: bool,
    skills: Vec<SkillBinding>,
    mentions: Vec<MentionBinding>,
    app_mentions: Vec<MentionBinding>,
    workspace_entries: Vec<CompletionCandidate>,
    completion_catalog: Vec<CompletionCandidate>,
    completion_mode: CompletionMode,
    completion_dismissed_text: Option<String>,
    selected_completion_bindings: Vec<SelectedCompletionBinding>,
    /// MCP servers that failed to start, reported before any picker was open.
    mcp_failures: Vec<(String, Option<String>)>,
    /// A session chosen while `thread/start` was still in flight. `thread/resume`
    /// needs a bound thread to switch away from, so the target waits here until the
    /// event loop can run it.
    deferred_resume: Option<DeferredResume>,
}

/// A session switch owed once the session being started exists, and whatever the
/// user typed after asking for it.
pub struct DeferredResume {
    pub target: String,
    /// Sending this before the switch would start a turn on the session being left,
    /// and `prepare_resume` drops the turn id locally without interrupting it — the
    /// turn would run on unwatched. So it travels with the switch instead.
    pub prompt: Option<String>,
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
        let vibe_mode = read_vibe_mode();
        let conversation_view = read_conversation_view();
        let (response_length, shell_display_mode, diff_display_mode) = match vibe_mode {
            VibeMode::Vibe => (ResponseLength::Short, ShellDisplayMode::Collapse, DiffDisplayMode::Collapse),
            VibeMode::SuperVibe => (ResponseLength::Short, ShellDisplayMode::Hide, DiffDisplayMode::Hide),
            VibeMode::Normal => (ResponseLength::Short, ShellDisplayMode::Expand, DiffDisplayMode::Expand),
        };
        let (five_hour_percent, weekly_percent) = read_codex_usage();
        let context_window = models
            .get(selected_model)
            .and_then(|model| model.context_window);

        Self {
            editor: Editor::default(),
            composer_images: Vec::new(),
            queued_prompts: VecDeque::new(),
            thread_id,
            turn_id: None,
            pending_interrupt: false,
            turn_interrupted: false,
            quit_armed: false,
            busy: false,
            cwd,
            account,
            models,
            selected_model,
            selected_effort,
            committed: Vec::new(),
            active_order: Vec::new(),
            active: HashMap::new(),
            shell_batches: HashMap::new(),
            turn_shell_results: Vec::new(),
            turn_shell_anchor: None,
            turn_shell_duration_ms: None,
            turn_file_changes: Vec::new(),
            turn_file_change_anchor: None,
            completed_item_ids: HashSet::new(),
            seen_operation_signatures: HashSet::new(),
            pending: None,
            context_tokens: 0,
            token_totals: TokenTotals::default(),
            cost_ledger: Some(CostLedger::default()),
            pending_turn_model: None,
            pending_turn_effort: None,
            active_turn_model: None,
            active_turn_effort: None,
            cost_restore_due: false,
            cost_restore_pending: false,
            context_window,
            transient_status: None,
            show_welcome: true,
            welcome_credits_expanded: false,
            plan_summary: None,
            command_selection: 0,
            spinner_frame: 0,
            turn_started_at: None,
            last_completed_duration: None,
            branch,
            five_hour_percent,
            weekly_percent,
            fast_mode: read_fast_mode(),
            side_parent: None,
            last_assistant_markdown: None,
            composer_notice: None,
            activity_notice: None,
            status_metadata_refreshed_at: Instant::now(),
            vibe_mode,
            conversation_view,
            response_length,
            shell_display_mode,
            diff_display_mode,
            status_line_settings: read_status_line_settings(),
            account_plan: AccountPlan::default(),
            account_refresh_due: false,
            skills: Vec::new(),
            mentions: Vec::new(),
            app_mentions: Vec::new(),
            workspace_entries: Vec::new(),
            completion_catalog: Vec::new(),
            completion_mode: CompletionMode::All,
            completion_dismissed_text: None,
            selected_completion_bindings: Vec::new(),
            mcp_failures: Vec::new(),
            deferred_resume: None,
        }
    }

    pub fn selected_model(&self) -> Option<&ModelInfo> {
        self.models.get(self.selected_model)
    }

    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    /// True until `thread/start` answers. The UI is fully painted before that, so
    /// anything that would talk to the thread has to wait for it.
    pub fn thread_pending(&self) -> bool {
        self.thread_id.is_empty()
    }

    /// Holds a session picked before the current one existed. The newest pick wins,
    /// but a prompt already typed for the switch stays with it: the user still means
    /// to send it, they only changed where.
    pub fn defer_resume(&mut self, target: String) {
        let prompt = self
            .deferred_resume
            .take()
            .and_then(|deferred| deferred.prompt);
        self.deferred_resume = Some(DeferredResume { target, prompt });
    }

    pub fn has_deferred_resume(&self) -> bool {
        self.deferred_resume.is_some()
    }

    /// Holds a prompt typed after a session was picked, joining anything already
    /// held the same way a second prompt joins the first during a wait.
    pub fn defer_prompt(&mut self, text: &str) {
        let Some(deferred) = self.deferred_resume.as_mut() else {
            return;
        };
        match deferred.prompt.as_mut() {
            Some(prompt) => {
                prompt.push_str("\n\n");
                prompt.push_str(text);
            }
            None => deferred.prompt = Some(text.to_owned()),
        }
    }

    /// Drops a held prompt without cancelling the switch it was going to ride on.
    pub fn cancel_deferred_prompt(&mut self) {
        if let Some(deferred) = self.deferred_resume.as_mut() {
            deferred.prompt = None;
        }
    }

    /// Hands the held switch over to be run, leaving nothing behind so it cannot
    /// happen twice.
    pub fn take_deferred_resume(&mut self) -> Option<DeferredResume> {
        self.deferred_resume.take()
    }

    /// Binds the session `thread/start` returned to a state that is already on
    /// screen. Unlike [`Self::set_thread`] it preserves what the user typed,
    /// submitted, or read while the request was in flight.
    pub fn attach_thread(
        &mut self,
        thread_id: String,
        cwd: String,
        model: &str,
        effort: Option<&str>,
    ) {
        self.thread_id = thread_id;
        if self.cwd != cwd {
            self.cwd = cwd;
            self.branch = read_git_branch(&self.cwd);
            self.workspace_entries.clear();
            self.rebuild_completion_catalog();
        }
        self.select_model_and_effort(model, effort);
    }

    pub fn note_pending_turn_model(&mut self, model: &str) {
        self.pending_turn_model = Some(model.to_owned());
    }

    pub fn note_pending_turn_effort(&mut self, effort: &str) {
        self.pending_turn_effort = Some(effort.to_owned());
    }

    /// Resumed history cannot be priced until its local rollout is restored.
    /// Keeping the ledger absent avoids charging all historical usage at the
    /// model currently selected in the picker.
    pub fn begin_cost_restore(&mut self) {
        self.cost_ledger = None;
        self.cost_restore_due = true;
        self.cost_restore_pending = true;
    }

    pub fn take_cost_restore(&mut self) -> Option<String> {
        self.cost_restore_due.then(|| {
            self.cost_restore_due = false;
            self.thread_id.clone()
        })
    }

    pub fn apply_restored_cost(&mut self, thread_id: &str, ledger: Option<CostLedger>) {
        if self.thread_id != thread_id {
            return;
        }
        self.cost_restore_pending = false;
        self.cost_ledger = ledger;
        let model = self.active_cost_model().to_owned();
        if let Some(ledger) = self.cost_ledger.as_mut() {
            ledger.record_cumulative(&model, self.token_totals);
        }
    }

    /// Puts the session back into its pending state so a switch can wipe the screen
    /// and keep repainting while `thread/start` runs, instead of freezing on the
    /// thread that is being replaced.
    pub fn begin_thread_switch(&mut self) {
        self.thread_id.clear();
        self.spinner_frame = 0;
    }

    /// Puts a failed switch back on the thread it left. Without this the session
    /// would stay pending forever, refusing every command that needs a thread.
    pub fn cancel_thread_switch(&mut self, previous_thread_id: String) {
        self.thread_id = previous_thread_id;
        self.show_welcome = true;
    }

    /// Drops a prompt that was queued before the thread existed, so Ctrl+C reads
    /// the same as interrupting a live turn.
    pub fn cancel_queued_prompt(&mut self) {
        self.last_completed_duration = self.turn_started_at.map(|started| started.elapsed());
        self.turn_interrupted = self.last_completed_duration.is_some();
        self.busy = false;
        self.turn_id = None;
        self.pending_interrupt = false;
        self.turn_started_at = None;
    }

    pub fn set_account_plan(&mut self, plan: AccountPlan) {
        self.account_plan = plan;
    }

    /// Picker keys must bypass the composer paste buffer so controls such as
    /// Space reach their pending interaction immediately.
    pub fn has_pending_interaction(&self) -> bool {
        self.pending.is_some()
    }

    pub fn update_skills(&mut self, response: &Value) {
        self.skills = parse_skill_bindings(response);
        self.rebuild_completion_catalog();
    }

    pub fn update_plugins(&mut self, response: &Value) {
        self.mentions = parse_plugin_mentions(response);
        self.rebuild_completion_catalog();
    }

    pub fn update_apps(&mut self, response: &Value) {
        self.app_mentions = parse_app_mentions(response);
        self.rebuild_completion_catalog();
    }

    pub fn update_workspace_entries(&mut self, entries: Vec<CompletionCandidate>) {
        self.workspace_entries = entries;
        self.rebuild_completion_catalog();
    }

    pub fn turn_input(&mut self, text: String) -> Vec<Value> {
        let triggers = mention_triggers(&text);
        let text_chars = text.chars().collect::<Vec<_>>();
        let mut input = vec![json!({
            "type": "text",
            "text": text,
            "text_elements": []
        })];
        for path in std::mem::take(&mut self.composer_images) {
            input.push(json!({ "type": "localImage", "path": path }));
        }
        let mut added_paths = Vec::new();
        let mut resolved_tokens = Vec::new();
        for binding in std::mem::take(&mut self.selected_completion_bindings) {
            if !binding.matches_text(&text_chars) || added_paths.contains(&binding.path) {
                continue;
            }
            input.push(binding.typed_item());
            added_paths.push(binding.path);
            resolved_tokens.push((binding.sigil, binding.trigger));
        }
        for skill in &self.skills {
            let matched = triggers.iter().find(|(sigil, trigger)| {
                *sigil == '$'
                    && trigger.eq_ignore_ascii_case(&skill.name)
                    && !resolved_tokens.iter().any(|resolved| {
                        resolved.0 == *sigil && resolved.1.eq_ignore_ascii_case(trigger)
                    })
            });
            if skill.enabled && matched.is_some() && !added_paths.contains(&skill.path) {
                input.push(json!({
                    "type": "skill",
                    "name": skill.name,
                    "path": skill.path
                }));
                added_paths.push(skill.path.clone());
                resolved_tokens.push(('$', skill.name.clone()));
            }
        }
        for mention in &self.mentions {
            let matched = triggers.iter().find(|(sigil, trigger)| {
                *sigil == '@'
                    && trigger.eq_ignore_ascii_case(&mention.trigger)
                    && !resolved_tokens.iter().any(|resolved| {
                        resolved.0 == *sigil && resolved.1.eq_ignore_ascii_case(trigger)
                    })
            });
            if matched.is_some() && !added_paths.contains(&mention.path) {
                input.push(json!({
                    "type": "mention",
                    "name": mention.name,
                    "path": mention.path
                }));
                added_paths.push(mention.path.clone());
                resolved_tokens.push(('@', mention.trigger.clone()));
            }
        }
        for mention in &self.app_mentions {
            let matched = triggers.iter().find(|(sigil, trigger)| {
                *sigil == '$'
                    && trigger.eq_ignore_ascii_case(&mention.trigger)
                    && !resolved_tokens.iter().any(|resolved| {
                        resolved.0 == *sigil && resolved.1.eq_ignore_ascii_case(trigger)
                    })
            });
            if matched.is_some() && !added_paths.contains(&mention.path) {
                input.push(json!({
                    "type": "mention",
                    "name": mention.name,
                    "path": mention.path
                }));
                added_paths.push(mention.path.clone());
                resolved_tokens.push(('$', mention.trigger.clone()));
            }
        }
        input
    }

    pub fn attach_local_image(&mut self, path: String) {
        if !self
            .composer_images
            .iter()
            .any(|attached| attached.eq_ignore_ascii_case(&path))
        {
            let index = self.editor.insert_attachment();
            self.composer_images.insert(index, path);
        }
    }

    #[allow(dead_code)]
    pub fn composer_image_count(&self) -> usize {
        self.composer_images.len()
    }

    pub fn confirm_plugin_install(&mut self, plugin: &PluginInfo) {
        let mut detail = vec![
            format!("Plugin: {}", plugin.display_name),
            format!("Marketplace: {}", plugin.marketplace_name),
        ];
        if let Some(description) = plugin
            .description
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            detail.push(description.to_owned());
        }
        detail.extend(plugin.install_disclosure());
        detail.push("설치하면 포함된 Skill, MCP 서버와 Hook이 Codex에 추가됩니다.".to_owned());
        self.pending = Some(PendingInteraction::Confirm {
            title: "플러그인을 설치할까요?".to_owned(),
            detail,
            action: ConfirmedAction::InstallPlugin(PluginInstallTarget {
                plugin_name: plugin.name.clone(),
                marketplace_path: plugin.marketplace_path.clone(),
                remote_marketplace_name: plugin.remote_marketplace_name.clone(),
            }),
        });
    }

    pub fn confirm_plugin_uninstall(&mut self, plugin: &PluginInfo) {
        self.pending = Some(PendingInteraction::Confirm {
            title: "플러그인을 제거할까요?".to_owned(),
            detail: vec![
                format!("Plugin: {}", plugin.display_name),
                format!("Marketplace: {}", plugin.marketplace_name),
                "포함된 Skill, MCP 서버와 Hook이 Codex에서 제거됩니다.".to_owned(),
            ],
            action: ConfirmedAction::UninstallPlugin(PluginUninstallTarget {
                plugin_id: plugin.id.clone(),
                display_name: plugin.display_name.clone(),
            }),
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

    pub fn permission_mode(&self) -> PermissionMode {
        PermissionMode::FullAccess
    }

    pub fn shell_display_mode(&self) -> ShellDisplayMode {
        self.shell_display_mode
    }

    pub fn diff_display_mode(&self) -> DiffDisplayMode {
        self.diff_display_mode
    }

    /// Permission profile id to send with `turn/start`.
    pub fn permission_profile(&self) -> &'static str {
        PermissionMode::FullAccess.profile()
    }

    pub fn response_length_label(&self) -> &'static str {
        self.response_length.label()
    }

    pub fn model_verbosity(&self) -> &'static str {
        self.response_length.model_verbosity()
    }

    fn composer_mode(&self) -> ComposerMode {
        ComposerMode {
            branch: self.branch.clone(),
            vibe_mode: self.vibe_mode.label().to_owned(),
            vibe_tone: match self.vibe_mode {
                VibeMode::Normal => VibeTone::Off,
                VibeMode::Vibe => VibeTone::On,
                VibeMode::SuperVibe => VibeTone::Super,
            },
            conversation_view: self.conversation_view.label().to_owned(),
            label: self.permission_mode().label().to_owned(),
            accent: self.permission_mode().accent(),
            model: self.selected_model_name().to_owned(),
            response_length: self.response_length_label().to_owned(),
            fast_mode: self.effective_fast_mode(),
            effort: self.selected_effort.clone(),
            shell_display_mode: self.shell_display_mode().label().to_owned(),
            diff_display_mode: self.diff_display_mode().label().to_owned(),
            cost: self.estimated_cost(),
        }
    }

    /// Estimated spend for the thread so far. `None` before the first turn
    /// reports usage, or when the model has no published rate.
    fn estimated_cost(&self) -> Option<String> {
        self.cost_ledger
            .as_ref()?
            .estimate_usd()
            .filter(|cost| *cost > 0.0)
            .map(pricing::format_usd)
    }

    fn active_cost_model(&self) -> &str {
        self.active_turn_model
            .as_deref()
            .unwrap_or_else(|| self.selected_model_name())
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

    /// Only the explicit toggle lands here, so it is the one place that owes the
    /// user a line: the badge alone leaves a `/fast` with no visible answer.
    pub fn set_fast_mode(&mut self, enabled: bool) {
        self.fast_mode = enabled;
        self.commit_welcome_card();
        self.committed.push(Block::new(
            BlockKind::ModelChange,
            if enabled {
                "✓ Fast mode On"
            } else {
                "✓ Fast mode Off"
            },
            "",
        ));
    }

    pub fn set_copy_notice(&mut self) {
        self.activity_notice = Some(("• Copied to clipboard".to_owned(), Instant::now()));
    }

    fn set_quit_notice(&mut self) {
        self.activity_notice = Some(("• Ctrl+C 한 번 더 누르면 종료합니다.".to_owned(), Instant::now()));
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
        self.reset_turn_item_tracking();
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
        if self.cwd != cwd {
            self.cwd = cwd;
            self.workspace_entries.clear();
            self.rebuild_completion_catalog();
        }
        self.turn_id = None;
        self.pending_interrupt = false;
        self.busy = false;
        self.turn_started_at = None;
        self.active.clear();
        self.active_order.clear();
        self.shell_batches.clear();
        self.reset_turn_item_tracking();
        self.show_welcome = true;
        self.select_model_and_effort(model, effort);
    }

    /// Points the status line at what the server actually picked, without the
    /// confirmation card [`Self::apply_model`] posts for a user-driven change.
    fn select_model_and_effort(&mut self, model: &str, effort: Option<&str>) {
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

    /// Rebuilds the transcript from a resumed thread. `rollout` fills in what
    /// `thread/resume` omits — shell runs above all — placing each one back where
    /// it ran rather than at the end of its turn.
    pub fn load_history(&mut self, thread: &Value, rollout: Option<&Rollout>) {
        let Some(turns) = thread.get("turns").and_then(Value::as_array) else {
            return;
        };
        self.turn_interrupted = false;
        self.last_completed_duration = turns.iter().rev().find_map(|turn| {
            let started = turn.get("startedAt")?.as_i64()?;
            let completed = turn.get("completedAt")?.as_i64()?;
            u64::try_from(completed.checked_sub(started)?)
                .ok()
                .map(Duration::from_secs)
        });
        for turn in turns {
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for block in merged_turn_blocks(&self.cwd, turn, items, rollout) {
                self.committed.push(block);
            }
            // Read straight off the server's own item order, not the
            // merged/sorted block order: a rollout event dated oddly must
            // never make an earlier message look like the last one shown.
            if let Some(text) = last_agent_message_text(items) {
                self.last_assistant_markdown = Some(text);
            }
        }
        if let Some(plan) = rollout.and_then(|rollout| rollout.last_plan.as_ref()) {
            self.restore_plan_snapshot(plan);
        }
        self.show_welcome = false;
    }

    fn restore_plan_snapshot(&mut self, plan: &PlanSnapshot) {
        self.plan_summary = Some(PlanSummary {
            explanation: plan.explanation.clone(),
            steps: plan
                .steps
                .iter()
                .map(|step| PlanStep {
                    text: step.text.clone(),
                    status: if step.status == "completed" {
                        PlanStepStatus::Completed
                    } else {
                        PlanStepStatus::Pending
                    },
                    started_at: None,
                    elapsed: step.elapsed_ms.map(Duration::from_millis),
                })
                .collect(),
            expanded: false,
            started_at: Instant::now(),
            elapsed: None,
        });
    }

    pub fn set_turn_started(&mut self, turn_id: String) {
        if self.turn_id.as_deref() != Some(turn_id.as_str()) {
            self.reset_turn_item_tracking();
        }
        self.turn_id = Some(turn_id);
        self.busy = true;
        if !self.pending_interrupt {
            self.turn_interrupted = false;
        }
        self.last_completed_duration = None;
        // A prompt held back while the session was still starting has been counting
        // since the user pressed Enter, so keep that clock rather than restarting it.
        self.turn_started_at.get_or_insert_with(Instant::now);
    }

    fn reset_turn_item_tracking(&mut self) {
        self.completed_item_ids.clear();
        self.seen_operation_signatures.clear();
        self.turn_shell_results.clear();
        self.turn_shell_anchor = None;
        self.turn_shell_duration_ms = None;
        self.turn_file_changes.clear();
        self.turn_file_change_anchor = None;
    }

    fn push_unique_operation(&mut self, block: Block) {
        if let Some(signature) = operation_signature(&block)
            && !self.seen_operation_signatures.insert(signature)
        {
            return;
        }
        push_latest_thinking(&mut self.committed, block);
    }

    /// Returns an interrupt the user requested while `turn/start` was still
    /// becoming active. The turn id is only populated by `turn/started`, so
    /// consuming this value cannot send an interrupt against a pending turn.
    pub fn take_pending_interrupt(&mut self) -> Option<String> {
        if self.pending_interrupt {
            let turn_id = self.turn_id.clone()?;
            self.pending_interrupt = false;
            Some(turn_id)
        } else {
            None
        }
    }

    pub fn set_request_failed(&mut self, message: impl Into<String>) {
        self.busy = false;
        self.turn_id = None;
        self.pending_interrupt = false;
        self.turn_interrupted = false;
        self.turn_started_at = None;
        self.committed
            .push(Block::new(BlockKind::Error, "요청 실패", message));
    }

    /// Interrupt an active turn immediately, or remember the request until the
    /// app-server announces that a just-started turn is active.
    fn request_interrupt(&mut self) -> Action {
        // Before `thread/start` binds a session, the main-loop startup helper
        // owns queued-prompt cancellation. Keep its established Ctrl+C/Esc
        // path rather than treating that as a pending turn.
        if self.thread_pending() {
            return Action::Interrupt;
        }
        if self.turn_id.is_some() {
            self.turn_interrupted = true;
            if self.last_completed_duration.is_none() {
                self.last_completed_duration =
                    self.turn_started_at.map(|started| started.elapsed());
            }
            return Action::Interrupt;
        }
        if !self.pending_interrupt {
            self.pending_interrupt = true;
            self.turn_interrupted = true;
            self.last_completed_duration = self.turn_started_at.map(|started| started.elapsed());
        }
        Action::Tick(true)
    }

    pub fn open_session_picker(&mut self, sessions: Vec<SessionInfo>) {
        self.pending = Some(PendingInteraction::SessionPicker(SessionPicker::new(
            sessions,
            self.cwd.clone(),
            Some(self.thread_id.clone()),
        )));
    }

    pub fn open_mcp_picker(&mut self, servers: Vec<McpServerInfo>, notice: Option<String>) {
        let mut picker = McpPicker::new(servers);
        if let Some(notice) = notice {
            picker = picker.with_notice(notice);
        }
        for (name, detail) in std::mem::take(&mut self.mcp_failures) {
            picker.apply_failure(&name, detail);
        }
        self.pending = Some(PendingInteraction::McpPicker(picker));
    }

    pub fn open_plugin_picker(
        &mut self,
        catalog: PluginCatalog,
        scope: Option<PluginScope>,
        notice: Option<String>,
    ) {
        let mut picker = PluginPicker::new(catalog, scope);
        if let Some(notice) = notice {
            picker = picker.with_notice(notice);
        }
        self.pending = Some(PendingInteraction::PluginPicker(picker));
    }

    pub fn open_plugin_detail(
        &mut self,
        catalog: PluginCatalog,
        target: PluginTarget,
        detail: PluginDetail,
        origin: Option<PluginScope>,
    ) {
        self.pending = Some(PendingInteraction::PluginPicker(
            PluginPicker::new(catalog, None).into_detail(target, detail, origin),
        ));
    }

    pub fn open_marketplace_picker(&mut self, catalog: &PluginCatalog, notice: Option<String>) {
        let mut picker = MarketplacePicker::new(catalog.marketplaces.clone());
        if let Some(notice) = notice {
            picker = picker.with_notice(notice);
        }
        self.pending = Some(PendingInteraction::MarketplacePicker(picker));
    }

    /// Records a failed MCP startup so the next `/mcp` lists it. The event loop
    /// sees these notifications while no picker is open, which is exactly when
    /// servers come up.
    pub fn note_mcp_failure(&mut self, name: String, detail: Option<String>) {
        if let Some(PendingInteraction::McpPicker(picker)) = self.pending.as_mut() {
            picker.apply_failure(&name, detail);
            return;
        }
        self.mcp_failures.retain(|(known, _)| known != &name);
        self.mcp_failures.push((name, detail));
    }

    pub fn confirm_marketplace_add(&mut self, source: &str) {
        self.pending = Some(PendingInteraction::Confirm {
            title: "마켓플레이스를 추가할까요?".to_owned(),
            detail: vec![
                format!("Source: {source}"),
                "Codex가 이 소스를 체크아웃하고 플러그인 목록을 읽습니다.".to_owned(),
                "신뢰할 수 있는 저장소만 추가하세요. 포함된 Hook과 MCP 서버는 설치 시 실행될 수 있습니다."
                    .to_owned(),
            ],
            action: ConfirmedAction::AddMarketplace(source.to_owned()),
        });
    }

    pub fn confirm_marketplace_remove(&mut self, name: &str) {
        self.pending = Some(PendingInteraction::Confirm {
            title: "마켓플레이스를 제거할까요?".to_owned(),
            detail: vec![
                format!("Marketplace: {name}"),
                "설정에서 소스만 제거하며, 이미 설치된 플러그인은 남습니다.".to_owned(),
            ],
            action: ConfirmedAction::RemoveMarketplace(name.to_owned()),
        });
    }

    pub fn prepare_resume(&mut self) {
        self.committed.clear();
        self.active.clear();
        self.active_order.clear();
        self.shell_batches.clear();
        self.reset_turn_item_tracking();
        self.pending = None;
        self.context_tokens = 0;
        self.token_totals = TokenTotals::default();
        self.cost_ledger = Some(CostLedger::default());
        self.pending_turn_model = None;
        self.pending_turn_effort = None;
        self.active_turn_model = None;
        self.active_turn_effort = None;
        self.cost_restore_due = false;
        self.cost_restore_pending = false;
        self.context_window = None;
        self.transient_status = None;
        self.side_parent = None;
        self.last_assistant_markdown = None;
        self.composer_notice = None;
        self.activity_notice = None;
        self.plan_summary = None;
        self.show_welcome = false;
        self.busy = false;
        self.turn_id = None;
        self.pending_interrupt = false;
        self.turn_interrupted = false;
        self.turn_started_at = None;
        self.last_completed_duration = None;
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

    /// Brings the welcome panel back after the screen is wiped, so a cleared
    /// terminal looks like a fresh start instead of a bare composer.
    pub fn reset_welcome(&mut self) {
        self.show_welcome = true;
        self.welcome_credits_expanded = false;
    }

    pub fn toggle_welcome_credits(&mut self) {
        self.welcome_credits_expanded = !self.welcome_credits_expanded;
    }

    pub fn drain_committed(&mut self) -> Vec<Block> {
        if self.show_welcome && !self.committed.is_empty() {
            let pending = std::mem::take(&mut self.committed);
            self.commit_welcome_card();
            self.committed.extend(pending);
        }
        let mut committed = std::mem::take(&mut self.committed);
        if self.shell_display_mode == ShellDisplayMode::Hide {
            // Inline transcript rows become permanent as soon as they are
            // handed to the renderer. Drop Shell and Web Search blocks here
            // so they cannot flash for one frame and disappear later.
            committed.retain(|block| !is_shell_hidden_block(block));
        }
        committed
    }

    pub fn view(&self) -> View<'_> {
        let mut operation_signatures = self.seen_operation_signatures.clone();
        let live_blocks = self
            .active_order
            .iter()
            .filter_map(|id| self.active.get(id))
            // A Shell item can briefly carry the app-server's running title
            // instead of its eventual Shell title. Its batch membership is the
            // stable discriminator, and keeps Hide from flashing it.
            .filter_map(|item| {
                if item.shell_batch.is_some()
                    || (self.shell_display_mode == ShellDisplayMode::Hide
                        && is_shell_hidden_block(&item.block))
                    || is_empty_thinking(&item.block)
                {
                    return None;
                }
                if let Some(signature) = operation_signature(&item.block)
                    && !operation_signatures.insert(signature)
                {
                    return None;
                }
                Some(item.block.clone())
            })
            .collect::<Vec<_>>();
        View {
            live_blocks,
            overlay: self.overlay_view(),
            plan_summary: self.plan_summary.as_ref(),
            plan_active: self.busy,
            editor: &self.editor,
            composer_images: &self.composer_images,
            queued_prompts: self.queued_prompts.iter().cloned().collect(),
            composer_placeholder: if self.busy {
                "Enter: steer · Tab: queue"
            } else {
                ""
            },
            welcome: self.show_welcome.then(|| self.welcome_view()),
            suggestions: if self.pending.is_none() {
                self.completion_suggestion_views()
                    .unwrap_or_else(|| self.slash_suggestion_views())
            } else {
                Vec::new()
            },
            activity: self.activity(),
            activity_model: self.activity_model(),
            activity_phase: self.activity_phase(),
            footer: self
                .status_line_has_content()
                .then(String::new)
                .unwrap_or_else(|| HIDDEN_STATUS_LINE.to_owned()),
            status_line: self.status_line_has_content().then(|| self.status_line()),
            composer_notice: self
                .composer_notice
                .as_ref()
                .map(|(notice, _)| notice.clone()),
            composer_mode: Some(self.composer_mode()),
            chat_layout: self.conversation_view.is_chat(),
            shell_display_mode: self.shell_display_mode(),
            diff_display_mode: self.diff_display_mode(),
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
        if self
            .activity_notice
            .as_ref()
            .is_some_and(|(_, shown_at)| shown_at.elapsed().as_millis() >= 1_400)
        {
            self.activity_notice = None;
            redraw = true;
        }
        redraw
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.handle_inserted_text(text, true);
    }

    pub fn handle_buffered_text(&mut self, text: &str) {
        self.handle_inserted_text(text, false);
    }

    fn handle_inserted_text(&mut self, text: &str, pasted: bool) {
        let old_text = self.editor.text();
        let binding_count = self.selected_completion_bindings.len();
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
            Some(PendingInteraction::McpPicker(picker)) => picker.handle_paste(text),
            Some(PendingInteraction::PluginPicker(picker)) => picker.handle_paste(text),
            Some(PendingInteraction::MarketplacePicker(picker)) => picker.handle_paste(text),
            Some(_) => {}
            None => {
                if pasted {
                    self.editor.insert_paste_str(text);
                } else {
                    self.editor.insert_str(text);
                }
                self.command_selection = 0;
            }
        }
        self.sync_selected_completion_bindings(&old_text, binding_count);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if !(key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.quit_armed = false;
        }
        let old_text = self.editor.text();
        let binding_count = self.selected_completion_bindings.len();
        let action = self.handle_key_inner(key);
        if !matches!(action, Action::Submit(_) | Action::Steer(_)) {
            self.sync_selected_completion_bindings(&old_text, binding_count);
        }
        action
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        // Windows Korean IMEs may turn one Ctrl+Backspace chord into a stream
        // of repeat records while dismantling a composed syllable. A word
        // delete must stay one atomic editor operation.
        if matches!(key.kind, KeyEventKind::Repeat)
            && ((key.code == KeyCode::Backspace
                && key.modifiers.contains(KeyModifiers::CONTROL))
                || key.code == KeyCode::Char('\u{8}'))
        {
            return Action::None;
        }
        if self.pending.is_some() {
            return self.handle_pending_key(key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if key.modifiers == KeyModifiers::SHIFT {
            match key.code {
                KeyCode::Up => {
                    self.move_selected_model(-1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.move_selected_model(1);
                    return Action::None;
                }
                KeyCode::Left => {
                    self.move_selected_effort(-1);
                    return Action::None;
                }
                KeyCode::Right => {
                    self.move_selected_effort(1);
                    return Action::None;
                }
                _ => {}
            }
        }

        if key.code == KeyCode::Esc && !self.busy {
            self.editor.clear();
            self.composer_images.clear();
            self.selected_completion_bindings.clear();
            self.completion_dismissed_text = None;
            self.command_selection = 0;
            return Action::None;
        }

        let completion_matches = self.matching_completions();
        if let Some((target, matches)) = completion_matches.as_ref() {
            if ctrl {
                match key.code {
                    KeyCode::Char('p') if !matches.is_empty() => {
                        self.command_selection = if self.command_selection == 0 {
                            matches.len() - 1
                        } else {
                            self.command_selection - 1
                        };
                        return Action::None;
                    }
                    KeyCode::Char('n') if !matches.is_empty() => {
                        self.command_selection = (self.command_selection + 1) % matches.len();
                        return Action::None;
                    }
                    _ => {}
                }
            }
            if !ctrl && !alt && !shift {
                match key.code {
                    KeyCode::Up => {
                        if !matches.is_empty() {
                            self.command_selection = if self.command_selection == 0 {
                                matches.len() - 1
                            } else {
                                self.command_selection - 1
                            };
                        }
                        return Action::None;
                    }
                    KeyCode::Down => {
                        if !matches.is_empty() {
                            self.command_selection = (self.command_selection + 1) % matches.len();
                        }
                        return Action::None;
                    }
                    KeyCode::Left if target.sigil == '@' => {
                        self.completion_mode = self.completion_mode.previous();
                        self.command_selection = 0;
                        return Action::None;
                    }
                    KeyCode::Right if target.sigil == '@' => {
                        self.completion_mode = self.completion_mode.next();
                        self.command_selection = 0;
                        return Action::None;
                    }
                    KeyCode::Tab | KeyCode::Enter => {
                        if let Some(selected) =
                            matches.get(self.command_selection.min(matches.len().saturating_sub(1)))
                        {
                            self.insert_completion(target, selected);
                        }
                        return Action::None;
                    }
                    KeyCode::Esc => {
                        self.completion_dismissed_text = Some(self.editor.text());
                        self.command_selection = 0;
                        return Action::None;
                    }
                    _ => {}
                }
            }
        }

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
            KeyCode::BackTab => Action::None,
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    if self.quit_armed {
                        Action::Quit
                    } else {
                        self.quit_armed = true;
                        self.set_quit_notice();
                        self.request_interrupt()
                    }
                } else if self.editor.is_empty()
                    && self.composer_images.is_empty()
                    && self.side_parent.is_some()
                {
                    Action::ReturnFromSide
                } else if self.editor.is_empty() && self.composer_images.is_empty() {
                    if self.quit_armed {
                        Action::Quit
                    } else {
                        self.quit_armed = true;
                        self.set_quit_notice();
                        Action::None
                    }
                } else {
                    self.editor.clear();
                    self.composer_images.clear();
                    Action::None
                }
            }
            KeyCode::Char('d')
                if ctrl
                    && self.editor.is_empty()
                    && self.composer_images.is_empty()
                    && !self.busy =>
            {
                Action::Quit
            }
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
            KeyCode::Tab if self.busy => self.queue_editor(),
            KeyCode::Enter => self.submit_editor(),
            KeyCode::Esc if self.busy => self.request_interrupt(),
            code if (code == KeyCode::Backspace && ctrl) || code == KeyCode::Char('\u{8}') => {
                if let Some(index) = self.editor.attachment_before_cursor() {
                    self.editor.delete_word_left();
                    self.composer_images.remove(index);
                } else {
                    self.editor.delete_word_left();
                }
                self.command_selection = 0;
                Action::None
            }
            KeyCode::Backspace => {
                if let Some(index) = self.editor.attachment_before_cursor() {
                    self.editor.backspace();
                    self.composer_images.remove(index);
                } else {
                    self.editor.backspace();
                }
                self.command_selection = 0;
                Action::None
            }
            KeyCode::Delete => {
                if let Some(index) = self.editor.attachment_at_cursor() {
                    self.editor.delete();
                    self.composer_images.remove(index);
                } else {
                    self.editor.delete();
                }
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
                if !self.editor.move_up() {
                    self.editor.history_previous();
                }
                Action::None
            }
            KeyCode::Down => {
                if !self.editor.move_down() {
                    self.editor.history_next();
                }
                Action::None
            }
            KeyCode::Char(ch) if !ctrl => {
                self.editor.insert(ch);
                self.command_selection = 0;
                if matches!(ch, '$' | '@') {
                    self.completion_mode = CompletionMode::All;
                }
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

    /// Whether the server is blocked on an answer from the user: an approval, a
    /// question, or an MCP prompt. The local overlays (`/model`, `/theme`, the
    /// session list) are the user's own detour and deliberately do not count —
    /// DevezCode raises its ❗ badge on this.
    pub fn awaiting_input(&self) -> bool {
        matches!(
            self.pending,
            Some(
                PendingInteraction::Approval { .. }
                    | PendingInteraction::UserInput { .. }
                    | PendingInteraction::McpForm(_)
                    | PendingInteraction::McpApproval(_)
                    | PendingInteraction::McpUrl { .. }
            )
        )
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
                    self.active_turn_model = self
                        .pending_turn_model
                        .take()
                        .or_else(|| Some(self.selected_model_name().to_owned()));
                    self.active_turn_effort = self
                        .pending_turn_effort
                        .take()
                        .or_else(|| Some(self.selected_effort.clone()));
                    self.set_turn_started(turn_id.to_owned());
                }
            }
            "turn/completed" => {
                self.busy = false;
                self.turn_id = None;
                self.pending_interrupt = false;
                if !self.turn_interrupted || self.last_completed_duration.is_none() {
                    self.last_completed_duration =
                        self.turn_started_at.map(|started| started.elapsed());
                }
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
            "turn/plan/updated" => {
                let explanation = params
                    .get("explanation")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty());
                let steps = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|step| {
                        let text = step.get("step")?.as_str()?;
                        let status = match step.get("status").and_then(Value::as_str) {
                            Some("completed") => PlanStepStatus::Completed,
                            Some("inProgress") => PlanStepStatus::InProgress,
                            _ => PlanStepStatus::Pending,
                        };
                        let previous = self
                            .plan_summary
                            .as_ref()
                            .and_then(|summary| {
                                summary.steps.iter().find(|previous| {
                                    previous.text == text
                                })
                            });
                        let started_at = match status {
                            PlanStepStatus::InProgress => previous
                                .and_then(|previous| previous.started_at)
                                .or_else(|| Some(Instant::now())),
                            PlanStepStatus::Completed => previous.and_then(|previous| previous.started_at),
                            PlanStepStatus::Pending => None,
                        };
                        let elapsed = if status == PlanStepStatus::Completed {
                            previous
                                .and_then(|previous| previous.elapsed)
                                .or_else(|| started_at.map(|started| started.elapsed()))
                                .or(Some(Duration::ZERO))
                        } else {
                            None
                        };
                        Some(PlanStep {
                            text: text.to_owned(),
                            status,
                            started_at,
                            elapsed,
                        })
                    })
                    .collect::<Vec<_>>();
                let started_at = self
                    .plan_summary
                    .as_ref()
                    .map(|summary| summary.started_at)
                    .unwrap_or_else(Instant::now);
                let expanded = self
                    .plan_summary
                    .as_ref()
                    .is_some_and(|summary| summary.expanded);
                let elapsed = if !steps.is_empty()
                    && steps.iter().all(|step| step.status == PlanStepStatus::Completed)
                {
                    self.plan_summary
                        .as_ref()
                        .and_then(|summary| summary.elapsed)
                        .or_else(|| Some(started_at.elapsed()))
                } else {
                    None
                };
                self.plan_summary = Some(PlanSummary {
                    explanation: explanation.map(ToOwned::to_owned),
                    steps,
                    expanded,
                    started_at,
                    elapsed,
                });
                self.show_welcome = false;
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
                self.append_shell_delta(params);
            }
            "item/plan/delta" => {
                self.append_delta(params, BlockKind::Reasoning, "Plan");
            }
            "item/fileChange/patchUpdated" => {
                if let Some(item_id) = params.get("itemId").and_then(Value::as_str) {
                    let changes = params
                        .get("changes")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let title = file_changes_title(&self.cwd, changes);
                    let body = file_changes_body(&self.cwd, changes);
                    let active = self.ensure_active(item_id, BlockKind::FileChange, &title);
                    active.block.title = title;
                    active.block.body = body;
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
                        let model = self.active_cost_model().to_owned();
                        if !self.cost_restore_pending {
                            if let Some(ledger) = self.cost_ledger.as_mut() {
                                ledger.record_cumulative(&model, self.token_totals);
                            }
                        }
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
            // The server rescans on every skill-file touch, so announcing it
            // would be constant noise. The catalogue still reloads; Codex says
            // nothing here either.
            "skills/changed" => {}
            // Like `skills/changed`, the rescan fires on every app-file touch,
            // so announcing it would be constant noise. The catalogue reloads
            // silently.
            "app/list/updated" => self.update_apps(params),
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
                if let Some((name, detail)) = crate::integrations::parse_startup_failure(params) {
                    self.committed.push(Block::new(
                        BlockKind::Warning,
                        format!("{name} unavailable"),
                        detail.clone(),
                    ));
                    // A server that never came up is absent from
                    // `mcpServerStatus/list`, so `/mcp` can only list it if the
                    // failure is remembered here.
                    self.note_mcp_failure(name, Some(detail));
                }
            }
            "thread/compacted" => self.push_unique_operation(Block::new(
                BlockKind::System,
                "Context compacted",
                "대화 컨텍스트가 압축되었습니다.",
            )),
            _ => {}
        }
    }

    fn submit_editor(&mut self) -> Action {
        let text = self.editor.take_for_submit().unwrap_or_default();
        self.submit_text(text)
    }

    pub fn start_queued_prompt(&mut self, text: String) -> Action {
        self.submit_text(text)
    }

    pub fn take_queued_prompt(&mut self) -> Option<String> {
        self.queued_prompts.pop_front()
    }

    pub fn remove_queued_prompt(&mut self, index: usize) -> bool {
        self.queued_prompts.remove(index).is_some()
    }

    fn queue_editor(&mut self) -> Action {
        if !self.composer_images.is_empty() {
            self.set_composer_notice("이미지 첨부 메시지는 Enter로 전송해주세요.".to_owned());
            return Action::None;
        }
        let text = self.editor.take_for_submit().unwrap_or_default();
        if text.is_empty() {
            return Action::None;
        }
        self.queued_prompts.push_back(text);
        Action::None
    }

    fn submit_text(&mut self, text: String) -> Action {
        if text.is_empty() && self.composer_images.is_empty() {
            return Action::None;
        }
        if text.starts_with('/') && !text.contains('\n') {
            return self.run_slash_command(&text);
        }
        self.commit_welcome_card();
        let display = if text.is_empty() {
            (1..=self.composer_images.len())
                .map(|index| format!("[Image #{index}]"))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            text.clone()
        };
        self.committed
            .push(Block::new(BlockKind::User, "You", display));
        if self.busy {
            Action::Steer(text)
        } else {
            self.reset_turn_item_tracking();
            self.busy = true;
            // Time the turn from Enter, not from the server's acknowledgement: a
            // prompt held back by a starting session would otherwise read 0s.
            self.turn_started_at = Some(Instant::now());
            Action::Submit(text)
        }
    }

    pub(crate) fn run_slash_command(&mut self, command: &str) -> Action {
        let parts = command.split_whitespace().collect::<Vec<_>>();
        match parts.first().copied().unwrap_or_default() {
            "/help" => {
                self.committed.push(Block::new(
                    BlockKind::System,
                    "Commands",
                    "/model [MODEL] [EFFORT]  모델과 effort 선택\n/fast [on|off]  Fast 서비스 티어 선택\n/effort [LEVEL]  추론 수준\n/shell [hide|collapse|expand]  Shell 표시 방식\n/diff [hide|collapse|expand]  Diff 표시 방식\n/theme [minimal|soft|dark]  화면 테마\n/statusline  하단 상태줄 항목 표시\n/mcp [reconnect|login NAME]  MCP 서버 탐색과 재연결\n/plugins [install|uninstall|enable|disable NAME]  플러그인 탐색과 관리\n/plugins marketplace [add SOURCE|remove NAME|upgrade]  마켓플레이스 관리\n/reload-plugins  플러그인 변경을 현재 세션에 적용\n/skills [enable|disable NAME]  Skill 관리\n/btw [MESSAGE]  임시 사이드 대화\n/compact  컨텍스트 압축\n/copy  마지막 답변 복사\n/resume [SESSION]  이전 세션 선택\n/continue  /resume 별칭\n/new  새 대화\n/login  ChatGPT 계정 로그인\n/logout  계정 연결 해제\n/status  현재 설정\n/usage  사용 한도\n/clear  화면 정리\n/quit  종료\n\n$  Plugin·Skill·App 검색\n@  Plugin·Skill·파일·폴더 검색\nEsc 또는 Ctrl+C  실행 중단\nCtrl+Enter / Shift+Enter  줄바꿈",
                ));
                Action::None
            }
            "/fast" if parts.len() == 1 => {
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
                    self.open_setting_picker(
                        DisplaySetting::Fast,
                        self.effective_fast_mode().then_some(0).unwrap_or(1),
                    );
                    Action::None
                }
            }
            "/fast" if parts.len() == 2 => {
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
                    match parts[1].to_ascii_lowercase().as_str() {
                        "on" => Action::SetFast(true),
                        "off" => Action::SetFast(false),
                        _ => {
                            self.committed.push(Block::new(
                                BlockKind::Error,
                                "Usage",
                                "/fast [on|off]",
                            ));
                            Action::None
                        }
                    }
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
            "/vibemode" if parts.len() == 1 => {
                self.pending = Some(PendingInteraction::VibeModePicker {
                    row: 0,
                    vibe: self.vibe_mode,
                    response: self.response_length,
                    shell: self.shell_display_mode,
                    diff: self.diff_display_mode,
                });
                Action::None
            }
            "/vibemode" => {
                self.committed.push(Block::new(BlockKind::Error, "Usage", "/vibemode"));
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
            "/mcp" if parts.len() == 1 => Action::OpenMcp(None),
            "/mcp" if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("login") => {
                Action::McpLogin(parts[2..].join(" "))
            }
            "/mcp" if parts.len() == 2 && parts[1].eq_ignore_ascii_case("reconnect") => {
                Action::ReconnectMcp
            }
            "/mcp" => {
                self.committed.push(Block::new(
                    BlockKind::Error,
                    "Usage",
                    "/mcp, /mcp reconnect 또는 /mcp login SERVER",
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
            "/plugins" if parts.len() == 1 => Action::OpenPlugins {
                scope: None,
                notice: None,
            },
            "/plugins"
                if parts.len() == 2
                    && (parts[1].eq_ignore_ascii_case("marketplace")
                        || parts[1].eq_ignore_ascii_case("marketplaces")) =>
            {
                Action::OpenMarketplaces(None)
            }
            "/plugins"
                if parts.len() >= 4
                    && parts[1].eq_ignore_ascii_case("marketplace")
                    && parts[2].eq_ignore_ascii_case("add") =>
            {
                Action::ConfirmMarketplaceAdd(parts[3..].join(" "))
            }
            "/plugins"
                if parts.len() >= 4
                    && parts[1].eq_ignore_ascii_case("marketplace")
                    && parts[2].eq_ignore_ascii_case("remove") =>
            {
                Action::ConfirmMarketplaceRemove(parts[3..].join(" "))
            }
            "/plugins"
                if parts.len() == 3
                    && parts[1].eq_ignore_ascii_case("marketplace")
                    && parts[2].eq_ignore_ascii_case("upgrade") =>
            {
                Action::UpgradeMarketplaces
            }
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
                    "/plugins, /plugins install|uninstall|enable|disable NAME\n\
                     /plugins marketplace [add SOURCE | remove NAME | upgrade]",
                ));
                Action::None
            }
            "/reload-plugins" | "/reload-skills" => Action::ReloadPlugins,
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
            "/shell" if parts.len() == 1 => {
                self.open_setting_picker(DisplaySetting::Shell, self.shell_display_mode as usize);
                Action::None
            }
            "/shell" if parts.len() == 2 => {
                self.set_display_setting(DisplaySetting::Shell, parts[1])
            }
            "/diff" if parts.len() == 1 => {
                self.open_setting_picker(DisplaySetting::Diff, self.diff_display_mode as usize);
                Action::None
            }
            "/diff" if parts.len() == 2 => self.set_display_setting(DisplaySetting::Diff, parts[1]),
            "/statusline" if parts.len() == 1 => {
                self.pending = Some(PendingInteraction::StatusLinePicker { selected: 0 });
                Action::None
            }
            "/statusline" => {
                self.committed
                    .push(Block::new(BlockKind::Error, "Usage", "/statusline"));
                Action::None
            }
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
                        self.permission_mode().label(),
                        self.permission_mode().profile(),
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
                            self.open_model_scope(index, self.effort_index_for_model(index));
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
                        self.open_model_scope(model_index, effort_index);
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
            PendingInteraction::ModelScope {
                model_index,
                effort_index,
                mut selected,
            } => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Up | KeyCode::Left => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                        selected = (selected + 1).min(ModelScope::CHOICES.len() - 1);
                    }
                    KeyCode::Char('k') if !ctrl && !alt => selected = selected.saturating_sub(1),
                    KeyCode::Char('j') if !ctrl && !alt => {
                        selected = (selected + 1).min(ModelScope::CHOICES.len() - 1);
                    }
                    KeyCode::Char(ch) if !ctrl && !alt && ('1'..='2').contains(&ch) => {
                        let index = ch.to_digit(10).unwrap_or(1) as usize - 1;
                        return self.apply_model_scope(
                            model_index,
                            effort_index,
                            ModelScope::CHOICES[index],
                        );
                    }
                    KeyCode::Enter => {
                        return self.apply_model_scope(
                            model_index,
                            effort_index,
                            ModelScope::CHOICES[selected],
                        );
                    }
                    _ => {}
                }
                self.pending = Some(PendingInteraction::ModelScope {
                    model_index,
                    effort_index,
                    selected,
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
            PendingInteraction::SettingPicker {
                setting,
                mut selected,
            } => {
                let count = setting.choices().len();
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Left | KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Char('p') if ctrl => selected = selected.saturating_sub(1),
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        selected = (selected + 1).min(count - 1);
                    }
                    KeyCode::Char('n') if ctrl => selected = (selected + 1).min(count - 1),
                    KeyCode::Char(ch) if !ctrl && !alt && ('1'..='9').contains(&ch) => {
                        let index = ch.to_digit(10).unwrap_or(1) as usize - 1;
                        if index < count {
                            return self.apply_setting_picker(setting, index);
                        }
                    }
                    KeyCode::Enter => return self.apply_setting_picker(setting, selected),
                    _ => {}
                }
                self.pending = Some(PendingInteraction::SettingPicker { setting, selected });
                Action::None
            }
            PendingInteraction::VibeModePicker {
                mut row,
                vibe,
                response,
                shell,
                diff,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.response_length = response;
                        self.shell_display_mode = shell;
                        self.diff_display_mode = diff;
                        self.vibe_mode = vibe;
                        return Action::None;
                    }
                    KeyCode::Enter => {
                        return Action::PersistVibeDisplayModes {
                            vibe: self.vibe_mode,
                            response: self.response_length,
                            shell: self.shell_display_mode,
                            diff: self.diff_display_mode,
                        };
                    }
                    KeyCode::Up => row = row.saturating_sub(1),
                    KeyCode::Down => row = (row + 1).min(2),
                    KeyCode::Left => match row {
                        0 => self.response_length = self.response_length.next().next(),
                        1 => self.shell_display_mode = self.shell_display_mode.next().next(),
                        _ => self.diff_display_mode = self.diff_display_mode.next().next(),
                    },
                    KeyCode::Right => match row {
                        0 => self.response_length = self.response_length.next(),
                        1 => self.shell_display_mode = self.shell_display_mode.next(),
                        _ => self.diff_display_mode = self.diff_display_mode.next(),
                    },
                    _ => {}
                }
                if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                    self.vibe_mode = VibeMode::Vibe;
                }
                self.pending = Some(PendingInteraction::VibeModePicker {
                    row,
                    vibe,
                    response,
                    shell,
                    diff,
                });
                Action::None
            }
            PendingInteraction::StatusLinePicker { mut selected } => match key.code {
                KeyCode::Esc => Action::None,
                KeyCode::Enter => Action::None,
                KeyCode::Up | KeyCode::Char('k') if !ctrl && !alt => {
                    selected = selected.saturating_sub(1);
                    self.pending = Some(PendingInteraction::StatusLinePicker { selected });
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') if !ctrl && !alt => {
                    selected = (selected + 1).min(StatusLineField::ALL.len() - 1);
                    self.pending = Some(PendingInteraction::StatusLinePicker { selected });
                    Action::None
                }
                KeyCode::Char(' ') => {
                    self.pending = Some(PendingInteraction::StatusLinePicker { selected });
                    self.toggle_status_line_field(StatusLineField::ALL[selected])
                }
                _ => {
                    self.pending = Some(PendingInteraction::StatusLinePicker { selected });
                    Action::None
                }
            },
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
            PendingInteraction::McpPicker(mut picker) => match picker.handle_key(key) {
                McpPickerResult::None => {
                    self.pending = Some(PendingInteraction::McpPicker(picker));
                    Action::None
                }
                McpPickerResult::Cancel => Action::None,
                McpPickerResult::Login(name) => Action::McpLogin(name),
                McpPickerResult::Reconnect => Action::ReconnectMcp,
            },
            PendingInteraction::PluginPicker(mut picker) => {
                let result = picker.handle_key(key);
                // Every branch that leaves the picker reopens it against fresh
                // data, so the scope it was on has to survive the round trip.
                let scope = picker.scope();
                match result {
                    PluginPickerResult::None => {
                        self.pending = Some(PendingInteraction::PluginPicker(picker));
                        Action::None
                    }
                    PluginPickerResult::Cancel => Action::None,
                    PluginPickerResult::OpenDetail(target) => Action::OpenPluginDetail {
                        target,
                        origin: scope,
                    },
                    PluginPickerResult::Install(plugin) => Action::ConfirmPluginInstall(plugin),
                    PluginPickerResult::Uninstall(plugin) => Action::ConfirmPluginUninstall(plugin),
                    PluginPickerResult::SetEnabled { plugin, enabled } => {
                        Action::SetPluginEnabled { plugin, enabled }
                    }
                    PluginPickerResult::OpenMarketplaces => Action::OpenMarketplaces(None),
                    PluginPickerResult::OpenUrl(url) => {
                        self.pending = Some(PendingInteraction::PluginPicker(picker));
                        Action::OpenUrl(url)
                    }
                }
            }
            PendingInteraction::MarketplacePicker(mut picker) => match picker.handle_key(key) {
                MarketplacePickerResult::None => {
                    self.pending = Some(PendingInteraction::MarketplacePicker(picker));
                    Action::None
                }
                MarketplacePickerResult::Cancel => Action::None,
                MarketplacePickerResult::Back => Action::OpenPlugins {
                    scope: None,
                    notice: None,
                },
                MarketplacePickerResult::Add(source) => Action::ConfirmMarketplaceAdd(source),
                MarketplacePickerResult::Remove(name) => Action::ConfirmMarketplaceRemove(name),
                MarketplacePickerResult::UpgradeAll => Action::UpgradeMarketplaces,
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
                let window = visible_window(Some(*model_index), self.models.len(), PICKER_ROWS);
                let start = window.start;
                let mut lines = self.models[window]
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
                let slider = self.models.get(*model_index).map(|model| {
                    lines.push(OverlayLine {
                        text: String::new(),
                        selected: false,
                        muted: true,
                    });
                    effort_slider(model, *effort_index)
                });
                Some(OverlayView {
                    closable: true,
                    title: "Model".to_owned(),
                    lines,
                    slider,
                    hint: "↑↓ model  ·  ←→ effort  ·  Enter to continue  ·  Esc to cancel"
                        .to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::ModelScope {
                model_index,
                effort_index,
                selected,
            } => {
                let model = self.models.get(*model_index)?;
                let effort = model
                    .efforts
                    .get(*effort_index)
                    .map(|effort| effort.id.as_str())
                    .unwrap_or(&model.default_effort);
                let label_width = ModelScope::CHOICES
                    .iter()
                    .map(|scope| scope.label().len())
                    .max()
                    .unwrap_or_default();
                let mut lines = vec![
                    OverlayLine {
                        text: format!("{}  ·  {effort}", model.display_name),
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
                    ModelScope::CHOICES
                        .iter()
                        .enumerate()
                        .map(|(index, scope)| OverlayLine {
                            text: format!(
                                "{}. {:<label_width$}  ·  {}",
                                index + 1,
                                scope.label(),
                                scope.detail()
                            ),
                            selected: index == *selected,
                            muted: false,
                        }),
                );
                Some(OverlayView {
                    closable: true,
                    title: "Apply to".to_owned(),
                    lines,
                    slider: None,
                    hint: "1-2 select  ·  ↑↓ navigate  ·  Enter to apply  ·  Esc to cancel"
                        .to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::EffortPicker { effort_index } => {
                let model = self.selected_model()?;
                Some(OverlayView {
                    closable: true,
                    title: "Effort".to_owned(),
                    lines: Vec::new(),
                    slider: Some(effort_slider(model, *effort_index)),
                    hint: "←→ to adjust  ·  Enter to confirm  ·  Esc to cancel".to_owned(),
                    style: OverlayStyle::Picker,
                    input: None,
                    input_label: "",
                    input_placeholder: "",
                })
            }
            PendingInteraction::SettingPicker { setting, selected } => Some(OverlayView {
                closable: true,
                title: setting.title().to_owned(),
                lines: Vec::new(),
                slider: Some(EffortSlider {
                    efforts: setting
                        .choices()
                        .iter()
                        .map(|choice| (*choice).to_owned())
                        .collect(),
                    selected: *selected,
                }),
                hint: "←→ to adjust  ·  Enter to confirm  ·  Esc to cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::VibeModePicker { row, .. } => Some(OverlayView {
                closable: true,
                title: "Vibe".to_owned(),
                lines: [
                    format!("Response: {}", self.response_length_label()),
                    format!("Shell: {}", self.shell_display_mode.label()),
                    format!("Diff: {}", self.diff_display_mode.label()),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, text)| OverlayLine {
                    text,
                    selected: index == *row,
                    muted: false,
                })
                .collect(),
                slider: None,
                hint: "↑↓ row  ·  ←→ adjust  ·  Enter to confirm  ·  Esc to cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::StatusLinePicker { selected } => Some(OverlayView {
                closable: true,
                title: "Status line".to_owned(),
                lines: StatusLineField::ALL
                    .iter()
                    .enumerate()
                    .map(|(index, field)| OverlayLine {
                        text: format!(
                            "{} {}",
                            if self.status_line_settings.enabled(*field) {
                                '☑'
                            } else {
                                '☐'
                            },
                            field.label()
                        ),
                        selected: index == *selected,
                        muted: false,
                    })
                    .collect(),
                slider: None,
                hint: "↑↓ navigate  ·  Space toggle  ·  Enter/Esc close".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::ThemePicker { theme_index } => Some(OverlayView {
                closable: false,
                title: "Theme".to_owned(),
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
                slider: None,
                hint: "1-3 select   ↑↓ navigate   Enter apply   Esc cancel".to_owned(),
                style: OverlayStyle::Picker,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::SessionPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::McpPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::PluginPicker(picker) => Some(picker.overlay_view()),
            PendingInteraction::MarketplacePicker(picker) => Some(picker.overlay_view()),
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
                    closable: false,
                    title: title.clone(),
                    lines,
                    slider: None,
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
                    closable: false,
                    title: "MCP approval".to_owned(),
                    lines,
                    slider: None,
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
                        // A server can offer far more options than fit, so the
                        // list scrolls with the cursor like the other pickers.
                        let offset = usize::from(!field.required);
                        let window = visible_window(
                            Some(form.selected.saturating_sub(offset)),
                            options.len(),
                            PICKER_ROWS,
                        );
                        let start = window.start;
                        lines.extend(options[window].iter().enumerate().map(
                            |(position, option)| OverlayLine {
                                text: option.label.clone(),
                                selected: start + position + offset == form.selected,
                                muted: false,
                            },
                        ));
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
                    closable: false,
                    title: field.title.clone(),
                    lines,
                    slider: None,
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
                closable: false,
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
                slider: None,
                hint: "o open   Enter continue   n decline   Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::LoginMethodPicker { selected } => Some(OverlayView {
                closable: false,
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
                slider: None,
                hint: "↑↓ select   Enter confirm   Esc cancel".to_owned(),
                style: OverlayStyle::Panel,
                input: None,
                input_label: "",
                input_placeholder: "",
            }),
            PendingInteraction::Login { waiting_on, .. } => Some(OverlayView {
                closable: false,
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
                slider: None,
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
                    closable: false,
                    title: title.clone(),
                    lines,
                    slider: None,
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
                    closable: false,
                    title: if question.header.is_empty() {
                        format!("Question {}/{}", current + 1, questions.len())
                    } else {
                        question.header.clone()
                    },
                    lines,
                    slider: None,
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
                category: None,
                panel_title: "Commands",
                hint: None,
            })
            .collect()
    }

    fn rebuild_completion_catalog(&mut self) {
        let mut candidates = Vec::new();
        let mut paths = Vec::new();
        for mention in &self.mentions {
            if paths.contains(&mention.path) {
                continue;
            }
            candidates.push(
                CompletionCandidate::new(
                    CompletionKind::Plugin,
                    &mention.name,
                    &mention.description,
                    completion_text(
                        CompletionKind::Plugin,
                        &mention.trigger,
                        Some(&mention.name),
                    ),
                )
                .with_binding(&mention.name, &mention.path),
            );
            paths.push(mention.path.clone());
        }
        for skill in &self.skills {
            if skill.enabled {
                candidates.push(
                    CompletionCandidate::new(
                        CompletionKind::Skill,
                        &skill.name,
                        &skill.description,
                        completion_text(CompletionKind::Skill, &skill.name, None),
                    )
                    .with_binding(&skill.name, &skill.path),
                );
            }
        }
        paths.clear();
        for mention in &self.app_mentions {
            if paths.contains(&mention.path) {
                continue;
            }
            candidates.push(
                CompletionCandidate::new(
                    CompletionKind::App,
                    &mention.name,
                    &mention.description,
                    completion_text(CompletionKind::App, &mention.trigger, None),
                )
                .with_binding(&mention.name, &mention.path),
            );
            paths.push(mention.path.clone());
        }
        candidates.extend(self.workspace_entries.iter().cloned());
        self.completion_catalog = candidates;
    }

    fn matching_completions(&self) -> Option<(CompletionTarget, Vec<CompletionCandidate>)> {
        let text = self.editor.text();
        if self.completion_dismissed_text.as_deref() == Some(text.as_str()) {
            return None;
        }
        let target = completion_target(&text, self.editor.cursor())?;
        let matches = filter_candidates(
            &self.completion_catalog,
            target.sigil,
            &target.query,
            self.completion_mode,
        )
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
        let shell_like = target.sigil == '$'
            && target
                .query
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit() || ch == '-');
        if shell_like && matches.is_empty() {
            return None;
        }
        Some((target, matches))
    }

    fn completion_suggestion_views(&self) -> Option<Vec<SuggestionView>> {
        let (target, matches) = self.matching_completions()?;
        let hint = (target.sigil == '@').then(|| {
            format!(
                "←/→ mode  ·  {}  ·  Enter/Tab insert  ·  Esc close",
                self.completion_mode.label()
            )
        });
        let panel_title = if target.sigil == '@' {
            "Mentions"
        } else {
            "Tools"
        };
        if matches.is_empty() {
            return Some(vec![SuggestionView {
                command: "No matches".to_owned(),
                description: String::new(),
                selected: false,
                category: None,
                panel_title,
                hint,
            }]);
        }
        let selected = self.command_selection.min(matches.len() - 1);
        Some(
            matches
                .into_iter()
                .enumerate()
                .map(|(index, candidate)| SuggestionView {
                    command: candidate.label,
                    description: candidate.description,
                    selected: index == selected,
                    category: Some(candidate.kind.label().to_owned()),
                    panel_title,
                    hint: hint.clone(),
                })
                .collect(),
        )
    }

    fn sync_selected_completion_bindings(&mut self, old_text: &str, existing_count: usize) {
        let new_text = self.editor.text();
        if old_text == new_text {
            return;
        }

        let old_chars = old_text.chars().collect::<Vec<_>>();
        let new_chars = new_text.chars().collect::<Vec<_>>();
        let prefix = old_chars
            .iter()
            .zip(&new_chars)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old_chars[prefix..]
            .iter()
            .rev()
            .zip(new_chars[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let old_edit_end = old_chars.len() - suffix;
        let new_edit_end = new_chars.len() - suffix;
        let shift = new_edit_end as isize - old_edit_end as isize;

        self.selected_completion_bindings = std::mem::take(&mut self.selected_completion_bindings)
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut binding)| {
                if index < existing_count {
                    if old_edit_end <= binding.range.start {
                        binding.range.start = binding.range.start.checked_add_signed(shift)?;
                        binding.range.end = binding.range.end.checked_add_signed(shift)?;
                    } else if prefix < binding.range.end {
                        return None;
                    }
                }
                binding.matches_text(&new_chars).then_some(binding)
            })
            .collect();
    }

    fn insert_completion(&mut self, target: &CompletionTarget, candidate: &CompletionCandidate) {
        self.editor
            .replace_range(target.range.clone(), &candidate.insert_text);
        let cursor = self.editor.cursor();
        let chars = self.editor.chars();
        let horizontal_separator = chars.get(cursor).is_some_and(|ch| {
            ch.is_whitespace() && !matches!(ch, '\n' | '\r' | '\u{000B}' | '\u{000C}')
        });
        if horizontal_separator {
            let has_suffix = chars.get(cursor + 1).is_some_and(|ch| !ch.is_whitespace());
            if has_suffix {
                self.editor.insert(' ');
            } else {
                self.editor.move_right();
            }
        } else {
            self.editor.insert(' ');
        }
        self.command_selection = 0;
        self.completion_dismissed_text = None;
        if let Some(binding) = candidate.binding.as_ref() {
            self.selected_completion_bindings
                .push(SelectedCompletionBinding {
                    sigil: candidate.insert_text.chars().next().unwrap_or(target.sigil),
                    trigger: candidate.insert_text.chars().skip(1).collect::<String>(),
                    token: candidate.insert_text.clone(),
                    range: target.range.start
                        ..target.range.start + candidate.insert_text.chars().count(),
                    kind: candidate.kind,
                    name: binding.name.clone(),
                    path: binding.path.clone(),
                });
        }
    }

    /// Nothing is shown while the session is still starting. The screen is already
    /// complete and the composer already works, so announcing the wait would only
    /// draw attention to a delay the user is about to spend typing through anyway.
    /// A prompt sent into that window still reports as `Working`, because it is.
    fn activity(&self) -> Option<String> {
        if let Some((notice, _)) = &self.activity_notice {
            return Some(notice.clone());
        }
        if self.busy {
            let elapsed = self
                .turn_started_at
                .map(|started| started.elapsed().as_secs())
                .unwrap_or(0);
            if self.turn_interrupted {
                return Some("X Interrupted".to_owned());
            }
            return Some(format!("Working.. ({})", format_elapsed(elapsed)));
        }
        if self.turn_interrupted && self.last_completed_duration.is_some() {
            return Some("X Interrupted".to_owned());
        }
        self.last_completed_duration
            .map(|duration| format!("Completed ({})", format_elapsed(duration.as_secs())))
    }

    fn activity_model(&self) -> Option<String> {
        if self.activity_notice.is_some() || (!self.busy && self.last_completed_duration.is_none()) {
            return None;
        }
        // The activity label is UI chrome, so it tracks the model currently
        // selected in the composer immediately. Billing keeps using the active
        // turn model separately in `active_cost_model`.
        Some(self.selected_model_name().to_owned())
    }

    /// The shimmer sweeps the `Working` label once per `SHIMMER_PERIOD`, read off
    /// the wall clock rather than counted in ticks so the glide keeps its pace no
    /// matter how often a frame happens to be painted.
    fn activity_phase(&self) -> f32 {
        let Some(started) = self.turn_started_at else {
            return 0.0;
        };
        let position = started.elapsed().as_millis() % SHIMMER_PERIOD.as_millis();
        position as f32 / SHIMMER_PERIOD.as_millis() as f32
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
            model: self
                .status_line_settings
                .enabled(StatusLineField::Model)
                .then(|| self.selected_model_display_name().to_owned()),
            effort: self
                .status_line_settings
                .enabled(StatusLineField::Effort)
                .then(|| self.selected_effort.clone()),
            context: self
                .status_line_settings
                .enabled(StatusLineField::Context)
                .then_some(context)
                .flatten(),
            five_hour_percent: self
                .status_line_settings
                .enabled(StatusLineField::FiveHour)
                .then_some(self.five_hour_percent)
                .flatten(),
            weekly_percent: self
                .status_line_settings
                .enabled(StatusLineField::Weekly)
                .then_some(self.weekly_percent)
                .flatten(),
            notice: self.transient_status.clone(),
        }
    }

    fn status_line_has_content(&self) -> bool {
        StatusLineField::ALL
            .iter()
            .any(|field| self.status_line_settings.enabled(*field))
            || self.transient_status.is_some()
    }

    /// Second step of `/model`: ask how long the pick lasts. A model with no
    /// choice to make would only add a keystroke, so this is always asked —
    /// persisting is destructive enough that it should never be the default.
    fn open_model_scope(&mut self, model_index: usize, effort_index: usize) {
        self.pending = Some(PendingInteraction::ModelScope {
            model_index,
            effort_index,
            selected: 0,
        });
    }

    fn apply_model_scope(
        &mut self,
        model_index: usize,
        effort_index: usize,
        scope: ModelScope,
    ) -> Action {
        let effort = self
            .models
            .get(model_index)
            .and_then(|model| model.efforts.get(effort_index))
            .map(|effort| effort.id.clone());
        self.apply_model(model_index, effort.as_deref());
        if scope == ModelScope::Session {
            return Action::None;
        }
        let Some(model) = self.models.get(model_index) else {
            return Action::None;
        };
        Action::PersistModelDefault {
            model: model.model.clone(),
            effort: self.selected_effort.clone(),
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

    fn move_selected_model(&mut self, direction: i8) {
        let next_index = match direction {
            -1 => self.selected_model.saturating_sub(1),
            1 => (self.selected_model + 1).min(self.models.len().saturating_sub(1)),
            _ => return,
        };
        if next_index == self.selected_model {
            return;
        }
        let model = self.models.get(next_index).map(|model| model.model.clone());
        let effort = self
            .models
            .get(next_index)
            .and_then(|model| model.efforts.get(self.effort_index_for_model(next_index)))
            .map(|effort| effort.id.clone());
        if let Some(model) = model {
            self.select_model_and_effort(&model, effort.as_deref());
            self.notice_setting_applies_to_next_request();
        }
    }

    fn move_selected_effort(&mut self, direction: i8) {
        let Some(model) = self.selected_model() else {
            return;
        };
        let current_index = model
            .efforts
            .iter()
            .position(|effort| effort.id == self.selected_effort)
            .unwrap_or(0);
        let next_index = match direction {
            -1 => current_index.saturating_sub(1),
            1 => (current_index + 1).min(model.efforts.len().saturating_sub(1)),
            _ => return,
        };
        let effort = model
            .efforts
            .get(next_index)
            .map(|effort| effort.id.clone());
        if next_index != current_index {
            if let Some(effort) = effort {
                self.selected_effort = effort;
                self.notice_setting_applies_to_next_request();
            }
        }
    }

    fn notice_setting_applies_to_next_request(&mut self) {
        if self.busy {
            self.set_composer_notice("Applies to the next request".to_owned());
        }
    }

    pub fn cycle_response_length(&mut self) {
        if self.pending.is_none() {
            self.response_length = self.response_length.next();
            self.vibe_mode = VibeMode::Vibe;
            self.notice_setting_applies_to_next_request();
        }
    }

    #[allow(dead_code)]
    pub fn vibe_mode_label(&self) -> &'static str {
        self.vibe_mode.label()
    }

    pub const fn vibe_mode(&self) -> VibeMode {
        self.vibe_mode
    }

    pub const fn response_length(&self) -> ResponseLength {
        self.response_length
    }

    pub fn cycle_conversation_view(&mut self) -> ConversationView {
        self.conversation_view = self.conversation_view.next();
        self.conversation_view
    }

    pub fn cycle_vibe_mode(&mut self) -> (ShellDisplayMode, DiffDisplayMode) {
        self.vibe_mode = self.vibe_mode.next();
        match self.vibe_mode {
            VibeMode::Vibe => {
                self.response_length = ResponseLength::Short;
                self.shell_display_mode = ShellDisplayMode::Collapse;
                self.diff_display_mode = DiffDisplayMode::Collapse;
            }
            VibeMode::SuperVibe => {
                self.response_length = ResponseLength::Short;
                self.shell_display_mode = ShellDisplayMode::Hide;
                self.diff_display_mode = DiffDisplayMode::Hide;
            }
            VibeMode::Normal => {
                self.response_length = ResponseLength::Short;
                self.shell_display_mode = ShellDisplayMode::Expand;
                self.diff_display_mode = DiffDisplayMode::Expand;
            }
        }
        self.notice_setting_applies_to_next_request();
        (self.shell_display_mode, self.diff_display_mode)
    }

    pub fn cycle_shell_display_mode(&mut self) -> ShellDisplayMode {
        self.shell_display_mode = self.shell_display_mode.next();
        self.vibe_mode = VibeMode::Vibe;
        self.shell_display_mode
    }

    pub fn cycle_diff_display_mode(&mut self) -> DiffDisplayMode {
        self.diff_display_mode = self.diff_display_mode.next();
        self.vibe_mode = VibeMode::Vibe;
        self.diff_display_mode
    }

    pub fn toggle_plan_summary(&mut self) {
        if let Some(summary) = &mut self.plan_summary {
            summary.expanded = !summary.expanded;
        }
    }

    /// Runs a slash command the composer never typed — what a click on the
    /// chrome that owns the same setting resolves to. Ignored while the session
    /// is blocked on an answer, so a stray click cannot swap the model out from
    /// under an approval that is still waiting. One of these pickers standing
    /// open is not such an answer: clicking the other reading switches straight
    /// to it, which is the whole point of the readings being clickable.
    pub fn run_command(&mut self, command: &str) -> Action {
        if self.pending.is_some() && !self.pending_is_model_family() {
            return Action::Tick(false);
        }
        self.run_slash_command(command)
    }

    /// Whether what is open is one of the model and effort pickers — the two a
    /// click on the status line may replace outright, since between them they
    /// only ever set the same two settings.
    fn pending_is_model_family(&self) -> bool {
        matches!(
            self.pending,
            Some(
                PendingInteraction::ModelPicker { .. }
                    | PendingInteraction::ModelScope { .. }
                    | PendingInteraction::EffortPicker { .. }
            )
        )
    }

    /// A click on a row inside an open picker. `row` is the position in
    /// `OverlayView::lines`, so each picker maps it back onto its own list — the
    /// window it scrolled to, or the header rows it printed first.
    pub fn click_overlay_row(&mut self, row: usize) -> Action {
        match self.pending.take() {
            Some(PendingInteraction::ModelPicker {
                model_index,
                effort_index,
            }) => {
                let start = visible_window(Some(model_index), self.models.len(), PICKER_ROWS).start;
                let clicked = start + row;
                if clicked < self.models.len() {
                    // The digit keys do exactly this: take the model and move on
                    // to the question of how long the pick lasts.
                    self.open_model_scope(clicked, self.effort_index_for_model(clicked));
                } else {
                    // The blank row under the list, or the slider's own rows.
                    self.pending = Some(PendingInteraction::ModelPicker {
                        model_index,
                        effort_index,
                    });
                }
                Action::None
            }
            Some(PendingInteraction::ModelScope {
                model_index,
                effort_index,
                selected,
            }) => {
                // The summary of the pick and the blank under it lead the rows.
                match row
                    .checked_sub(MODEL_SCOPE_HEADER_ROWS)
                    .and_then(|choice| ModelScope::CHOICES.get(choice))
                {
                    Some(scope) => self.apply_model_scope(model_index, effort_index, *scope),
                    None => {
                        self.pending = Some(PendingInteraction::ModelScope {
                            model_index,
                            effort_index,
                            selected,
                        });
                        Action::Tick(false)
                    }
                }
            }
            Some(PendingInteraction::ThemePicker { theme_index }) => {
                match ThemeKind::ALL.get(row) {
                    Some(theme) => self.apply_theme(*theme),
                    None => {
                        self.pending = Some(PendingInteraction::ThemePicker { theme_index });
                        Action::Tick(false)
                    }
                }
            }
            Some(PendingInteraction::StatusLinePicker { .. })
                if row < StatusLineField::ALL.len() =>
            {
                self.pending = Some(PendingInteraction::StatusLinePicker { selected: row });
                self.toggle_status_line_field(StatusLineField::ALL[row])
            }
            Some(PendingInteraction::SessionPicker(mut picker)) => match picker.click_row(row) {
                SessionPickerResult::Select(thread_id) => Action::ResumeThread(thread_id),
                SessionPickerResult::Cancel => Action::None,
                SessionPickerResult::None => {
                    self.pending = Some(PendingInteraction::SessionPicker(picker));
                    Action::Tick(false)
                }
            },
            other => {
                self.pending = other;
                Action::Tick(false)
            }
        }
    }

    /// The `✕` on a panel the user opened themselves: closes it, exactly as Esc
    /// does. Only the panels that paint the mark can be shut this way, so a prompt
    /// the server is waiting on stays put whatever is clicked.
    pub fn close_overlay(&mut self) -> Action {
        match self.pending.take() {
            Some(pending) if closable_overlay(&pending) => Action::None,
            other => {
                self.pending = other;
                Action::Tick(false)
            }
        }
    }

    /// A click on one step of an effort track. In the model picker the track is a
    /// control beside the list, so the click only moves it; the effort picker has
    /// nothing else to answer for, so a click there settles it.
    pub fn click_effort_step(&mut self, step: usize) -> Action {
        match self.pending.take() {
            Some(PendingInteraction::ModelPicker { model_index, .. }) => {
                let count = self
                    .models
                    .get(model_index)
                    .map(|model| model.efforts.len())
                    .unwrap_or(1)
                    .max(1);
                self.pending = Some(PendingInteraction::ModelPicker {
                    model_index,
                    effort_index: step.min(count - 1),
                });
                Action::None
            }
            Some(PendingInteraction::EffortPicker { effort_index }) => {
                let effort = self
                    .selected_model()
                    .and_then(|model| model.efforts.get(step))
                    .map(|effort| effort.id.clone());
                match effort {
                    Some(effort) => {
                        self.apply_effort(&effort);
                        Action::None
                    }
                    None => {
                        self.pending = Some(PendingInteraction::EffortPicker { effort_index });
                        Action::Tick(false)
                    }
                }
            }
            Some(PendingInteraction::SettingPicker { setting, selected }) => {
                if step < setting.choices().len() {
                    self.apply_setting_picker(setting, step)
                } else {
                    self.pending = Some(PendingInteraction::SettingPicker { setting, selected });
                    Action::Tick(false)
                }
            }
            other => {
                self.pending = other;
                Action::Tick(false)
            }
        }
    }

    fn open_setting_picker(&mut self, setting: DisplaySetting, selected: usize) {
        self.pending = Some(PendingInteraction::SettingPicker {
            setting,
            selected: selected.min(setting.choices().len().saturating_sub(1)),
        });
    }

    fn toggle_status_line_field(&mut self, field: StatusLineField) -> Action {
        let enabled = self.status_line_settings.toggle(field);
        Action::PersistStatusLine {
            key_path: field.config_key(),
            enabled,
        }
    }

    fn set_display_setting(&mut self, setting: DisplaySetting, value: &str) -> Action {
        let Some(selected) = setting
            .choices()
            .iter()
            .position(|choice| choice.eq_ignore_ascii_case(value))
        else {
            self.committed.push(Block::new(
                BlockKind::Error,
                "Usage",
                format!(
                    "/{} [{}]",
                    setting.title().to_ascii_lowercase(),
                    setting.choices().join("|")
                ),
            ));
            return Action::None;
        };
        self.apply_setting_picker(setting, selected)
    }

    fn apply_setting_picker(&mut self, setting: DisplaySetting, selected: usize) -> Action {
        match setting {
            DisplaySetting::Shell => {
                let mode = match selected {
                    0 => ShellDisplayMode::Hide,
                    1 => ShellDisplayMode::Collapse,
                    2 => ShellDisplayMode::Expand,
                    _ => self.shell_display_mode,
                };
                self.shell_display_mode = mode;
                self.vibe_mode = VibeMode::Vibe;
                Action::PersistShellDisplayMode(mode)
            }
            DisplaySetting::Diff => {
                let mode = match selected {
                    0 => DiffDisplayMode::Hide,
                    1 => DiffDisplayMode::Collapse,
                    2 => DiffDisplayMode::Expand,
                    _ => self.diff_display_mode,
                };
                self.diff_display_mode = mode;
                self.vibe_mode = VibeMode::Vibe;
                Action::PersistDiffDisplayMode(mode)
            }
            DisplaySetting::Fast => Action::SetFast(selected == 0),
        }
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
            credits_expanded: self.welcome_credits_expanded,
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
        if self.completed_item_ids.contains(id) {
            return;
        }
        let Some(mut block) = active_item_block(&self.cwd, item) else {
            return;
        };
        let existing_batch = self
            .active
            .get(id)
            .and_then(|existing| existing.shell_batch.clone());
        let was_active = self.active.contains_key(id);
        if let Some(existing) = self.active.get(id) {
            block.adopt_id(&existing.block);
            block.body = existing.block.body.clone();
        }
        let shell_batch = if existing_batch.is_some() {
            existing_batch
        } else if block.title.starts_with("Shell ·") {
            self.active_order
                .iter()
                .filter_map(|active_id| self.active.get(active_id))
                .find_map(|active| active.shell_batch.clone())
                .or_else(|| Some(id.to_owned()))
        } else {
            None
        };
        if !was_active {
            self.active_order.push(id.to_owned());
            if let Some(batch_id) = shell_batch.as_ref() {
                self.register_shell_member(batch_id, id, &block);
            }
        }
        self.active
            .insert(id.to_owned(), ActiveItem { block, shell_batch });
    }

    fn complete_item(&mut self, item: &Value) {
        let id = item.get("id").and_then(Value::as_str);
        if let Some(id) = id
            && !self.completed_item_ids.insert(id.to_owned())
        {
            return;
        }
        let active = id.and_then(|id| {
            let active = self.active.remove(id);
            self.active_order.retain(|candidate| candidate != id);
            active
        });
        if item.get("type").and_then(Value::as_str) == Some("userMessage") {
            return;
        }
        if let Some(mut block) = completed_item_block(&self.cwd, item) {
            if let Some(active) = active.as_ref() {
                block.adopt_id(&active.block);
                if block.body.is_empty() {
                    block.body = active.block.body.clone();
                }
            }
            if let (Some(id), Some(batch_id)) = (
                id,
                active
                    .as_ref()
                    .and_then(|active| active.shell_batch.as_deref()),
            ) && block.title.starts_with("Shell ·")
            {
                self.complete_shell_batch_member(
                    batch_id.to_owned(),
                    id.to_owned(),
                    ShellResult {
                        block,
                        exit_code: item.get("exitCode").and_then(Value::as_i64),
                        duration_ms: item.get("durationMs").and_then(Value::as_u64),
                    },
                );
                return;
            }
            if matches!(block.kind, BlockKind::Assistant) {
                self.last_assistant_markdown = Some(block.body.clone());
            }
            if matches!(block.kind, BlockKind::FileChange) {
                if let Some(signature) = operation_signature(&block)
                    && !self.seen_operation_signatures.insert(signature)
                {
                    return;
                }
                self.commit_turn_file_change(block);
                return;
            }
            self.push_unique_operation(block);
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

    /// Command output can arrive before `item/started`. Mark it as Shell at the
    /// first byte so Shell: Hide never paints the generic running Tool frame.
    fn append_shell_delta(&mut self, params: &Value) {
        let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
            return;
        };
        let Some(delta) = params.get("delta").and_then(Value::as_str) else {
            return;
        };
        self.mark_active_shell(item_id);
        append_capped(
            &mut self
                .active
                .get_mut(item_id)
                .expect("shell output is active")
                .block
                .body,
            delta,
        );
    }

    fn mark_active_shell(&mut self, item_id: &str) {
        let existing_batch = self
            .active
            .get(item_id)
            .and_then(|active| active.shell_batch.clone());
        if existing_batch.is_some() {
            return;
        }
        let batch_id = self
            .active_order
            .iter()
            .filter_map(|active_id| self.active.get(active_id))
            .find_map(|active| active.shell_batch.clone())
            .unwrap_or_else(|| item_id.to_owned());
        if !self.active.contains_key(item_id) {
            self.active_order.push(item_id.to_owned());
            self.active.insert(
                item_id.to_owned(),
                ActiveItem {
                    block: Block::new(BlockKind::Tool, "Shell · command", ""),
                    shell_batch: None,
                },
            );
        }
        let active = self.active.get_mut(item_id).expect("active shell exists");
        active.shell_batch = Some(batch_id.clone());
        active.block.title = "Shell · command".to_owned();

        let block = active.block.clone();
        self.register_shell_member(&batch_id, item_id, &block);
    }

    fn register_shell_member(&mut self, batch_id: &str, item_id: &str, source: &Block) {
        if !self.shell_batches.contains_key(batch_id) {
            let mut anchor = self.turn_shell_anchor.clone().unwrap_or_else(|| {
                let mut anchor = Block::new(BlockKind::Tool, "", "");
                anchor.adopt_id(source);
                anchor
            });
            anchor.kind = BlockKind::Tool;
            self.turn_shell_anchor = Some(anchor.clone());
            self.shell_batches.insert(
                batch_id.to_owned(),
                ShellBatch {
                    anchor,
                    members: Vec::new(),
                    completed: HashMap::new(),
                },
            );
        }

        let batch = self
            .shell_batches
            .get_mut(batch_id)
            .expect("shell batch inserted");
        if !batch.members.iter().any(|member| member == item_id) {
            batch.members.push(item_id.to_owned());
        }
        let count = self.turn_shell_results.len() + batch.members.len();
        let noun = if count == 1 { "command" } else { "commands" };
        batch.anchor.title = format!("Running {count} shell {noun}");
        batch.anchor.kind = BlockKind::Tool;
        let anchor = batch.anchor.clone();
        self.turn_shell_anchor = Some(anchor.clone());
        self.commit_replacing(anchor);
    }

    fn commit_replacing(&mut self, block: Block) {
        if let Some(existing) = self
            .committed
            .iter_mut()
            .find(|existing| existing.id() == block.id())
        {
            *existing = block;
        } else {
            self.committed.push(block);
        }
    }

    fn commit_turn_file_change(&mut self, block: Block) {
        self.turn_file_changes.push(block);
        let mut grouped = file_change_group_block(self.turn_file_changes.clone());
        if let Some(anchor) = self.turn_file_change_anchor.as_ref() {
            grouped.adopt_id(anchor);
        }
        self.turn_file_change_anchor = Some(grouped.clone());
        self.commit_replacing(grouped);
    }

    fn ensure_active(&mut self, item_id: &str, kind: BlockKind, title: &str) -> &mut ActiveItem {
        if !self.active.contains_key(item_id) {
            self.active_order.push(item_id.to_owned());
            self.active.insert(
                item_id.to_owned(),
                ActiveItem {
                    block: Block::new(kind, title, ""),
                    shell_batch: None,
                },
            );
        }
        self.active.get_mut(item_id).expect("inserted")
    }

    fn complete_shell_batch_member(
        &mut self,
        batch_id: String,
        item_id: String,
        result: ShellResult,
    ) {
        let Some(batch) = self.shell_batches.get_mut(&batch_id) else {
            self.committed.push(result.block);
            return;
        };
        batch.completed.insert(item_id, result);
        if batch.completed.len() != batch.members.len() {
            return;
        }

        let mut batch = self
            .shell_batches
            .remove(&batch_id)
            .expect("completed batch exists");
        let results = batch
            .members
            .iter()
            .filter_map(|id| batch.completed.remove(id))
            .collect::<Vec<_>>();
        self.commit_turn_shell_results(results, &batch.anchor);
    }

    fn commit_turn_shell_results(&mut self, results: Vec<ShellResult>, anchor: &Block) {
        if results.is_empty() {
            return;
        }
        if let Some(batch_duration) = results.iter().filter_map(|result| result.duration_ms).max() {
            self.turn_shell_duration_ms = Some(
                self.turn_shell_duration_ms
                    .unwrap_or(0)
                    .saturating_add(batch_duration),
            );
        }
        self.turn_shell_results.extend(results);
        let mut completed = shell_results_block_with_duration(
            self.turn_shell_results.clone(),
            self.turn_shell_duration_ms,
        );
        completed.adopt_id(self.turn_shell_anchor.as_ref().unwrap_or(anchor));
        self.turn_shell_anchor = Some(completed.clone());
        self.commit_replacing(completed);
    }

    fn flush_orphaned_active(&mut self) {
        let mut shell_updates = Vec::new();
        for (_, mut batch) in std::mem::take(&mut self.shell_batches) {
            let mut results = Vec::new();
            for id in &batch.members {
                if let Some(result) = batch.completed.remove(id) {
                    results.push(result);
                    continue;
                }
                if let Some(item) = self.active.remove(id) {
                    self.active_order.retain(|candidate| candidate != id);
                    results.push(ShellResult {
                        block: item.block,
                        exit_code: None,
                        duration_ms: None,
                    });
                }
            }
            if !results.is_empty() {
                shell_updates.push((results, batch.anchor));
            }
        }
        for (results, anchor) in shell_updates {
            self.commit_turn_shell_results(results, &anchor);
        }
        for id in std::mem::take(&mut self.active_order) {
            if let Some(item) = self.active.remove(&id) {
                if matches!(item.block.kind, BlockKind::Assistant) {
                    self.last_assistant_markdown = Some(item.block.body.clone());
                }
                if matches!(item.block.kind, BlockKind::FileChange) {
                    if operation_signature(&item.block)
                        .is_none_or(|signature| self.seen_operation_signatures.insert(signature))
                    {
                        self.commit_turn_file_change(item.block);
                    }
                } else {
                    self.push_unique_operation(item.block);
                }
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

fn active_item_block(cwd: &str, item: &Value) -> Option<Block> {
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
                "Shell · {}",
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
        "fileChange" => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            Some(Block::new(
                BlockKind::FileChange,
                file_changes_title(cwd, changes),
                file_changes_body(cwd, changes),
            ))
        }
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
        "webSearch" => {
            let query = compact_command(
                item.get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                88,
            );
            Some(Block::new(
                BlockKind::Tool,
                if query.is_empty() {
                    "Web search".to_owned()
                } else {
                    format!("Web search · {query}")
                },
                "",
            ))
        }
        "collabAgentToolCall" => Some(Block::new(
            BlockKind::Tool,
            "Agent",
            item.get("tool").map(Value::to_string).unwrap_or_default(),
        )),
        _ => None,
    }
}

fn is_thinking(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Reasoning) && block.title == "Thinking…"
}

fn is_empty_thinking(block: &Block) -> bool {
    is_thinking(block) && block.body.trim().is_empty()
}

fn is_running_shell_block(block: &Block) -> bool {
    let text = format!("{}\n{}", block.title, block.body).to_ascii_lowercase();
    text.contains("running") && text.contains("shell") && text.contains("command")
}

fn is_shell_block(block: &Block) -> bool {
    block.title.starts_with("Shell ·") || is_running_shell_block(block)
}

fn is_web_search_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Tool)
        && (block.title == "Web search" || block.title.starts_with("Web search ·"))
}

fn is_auxiliary_tool_block(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Tool)
        && (block.title.starts_with("MCP ·")
            || block.title.starts_with("Tool ·")
            || block.title == "Agent")
}

fn is_shell_hidden_block(block: &Block) -> bool {
    is_shell_block(block) || is_web_search_block(block) || is_auxiliary_tool_block(block)
}

/// Operations whose repeated cards add no information. The body participates
/// in the signature, so two calls to the same tool with different results stay
/// visible; Web Search includes its query in the title for the same reason.
fn operation_signature(block: &Block) -> Option<String> {
    if matches!(block.kind, BlockKind::System) && block.title == "Context compacted" {
        return Some("context-compaction".to_owned());
    }
    let (family, include_title) = match block.kind {
        BlockKind::Tool
            if block.title == "Web search"
                || block.title.starts_with("Web search ·")
                || block.title.starts_with("MCP ·")
                || block.title.starts_with("Tool ·")
                || block.title == "Agent" =>
        {
            ("tool", true)
        }
        BlockKind::FileChange => ("file-change", true),
        BlockKind::Plan => ("plan", false),
        BlockKind::Reasoning if block.title == "Plan" => ("plan", false),
        _ => return None,
    };
    Some(if include_title {
        format!("{family}\0{}\0{}", block.title, block.body)
    } else {
        format!("{family}\0{}", block.body)
    })
}

fn push_latest_thinking(blocks: &mut Vec<Block>, block: Block) {
    if is_empty_thinking(&block) {
        return;
    }
    if is_thinking(&block) && blocks.last().is_some_and(is_thinking) {
        blocks.pop();
    }
    blocks.push(block);
}

fn normalized_turn_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut seen_operations = HashSet::new();
    let normalized = blocks
        .into_iter()
        .fold(Vec::new(), |mut normalized, block| {
            if let Some(signature) = operation_signature(&block)
                && !seen_operations.insert(signature)
            {
                return normalized;
            }
            push_latest_thinking(&mut normalized, block);
            normalized
        });
    group_turn_file_changes(normalized)
}

fn file_change_group_block(children: Vec<Block>) -> Block {
    let title = children
        .first()
        .filter(|first| children.iter().all(|block| block.title == first.title))
        .map(|block| block.title.clone())
        .unwrap_or_else(|| format!("Update({} changes)", children.len()));
    Block::file_change_group(title, children)
}

fn group_turn_file_changes(blocks: Vec<Block>) -> Vec<Block> {
    let mut grouped: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut file_changes = Vec::new();
    let mut group_index = None;
    for block in blocks {
        if matches!(block.kind, BlockKind::FileChange) {
            file_changes.push(block);
            let group = file_change_group_block(file_changes.clone());
            if let Some(index) = group_index {
                grouped[index] = group;
            } else {
                group_index = Some(grouped.len());
                grouped.push(group);
            }
        } else {
            grouped.push(block);
        }
    }
    grouped
}

fn completed_item_block(cwd: &str, item: &Value) -> Option<Block> {
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
                    "Shell · {}{suffix}{duration}",
                    compact_command(
                        item.get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("command"),
                        88
                    )
                ),
                // The renderer shows only the last few rows and counts the rest,
                // so this cap is a memory guard: keep it high enough that the
                // count it reports is the real one for any ordinary command.
                collapse_output(
                    &strip_ansi(
                        item.get("aggregatedOutput")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    400,
                ),
            ))
        }
        "fileChange" => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            Some(Block::new(
                BlockKind::FileChange,
                file_changes_title(cwd, changes),
                file_changes_body(cwd, changes),
            ))
        }
        "mcpToolCall" => {
            let mut block = active_item_block(cwd, item)?;
            block.body = match item.get("error").filter(|value| !value.is_null()) {
                Some(error) => pretty_json(Some(error)),
                None => pretty_json(item.get("result")),
            };
            Some(block)
        }
        "dynamicToolCall" => {
            let mut block = active_item_block(cwd, item)?;
            block.body = pretty_json(item.get("contentItems"));
            Some(block)
        }
        "contextCompaction" => Some(Block::new(BlockKind::System, "Context compacted", "")),
        _ => active_item_block(cwd, item),
    }
}

/// One turn's blocks, server items and rollout events interleaved by time.
fn merged_turn_blocks(
    cwd: &str,
    turn: &Value,
    items: &[Value],
    rollout: Option<&Rollout>,
) -> Vec<Block> {
    let events = rollout
        .map(|rollout| turn_events(turn, rollout))
        .unwrap_or_default();
    let mut rows: Vec<(String, usize, Block)> = Vec::new();
    let mut order = 0usize;
    // Items the rollout cannot date — user messages, MCP calls — inherit the
    // last known time so they keep their place relative to what surrounds
    // them. Seeding this with the turn's own start (rather than `""`) keeps a
    // turn with no anchored items at all from sorting its server items ahead
    // of every `Bash` block, which does carry a real timestamp — exactly the
    // "all shells bunched at the end" shape the merge exists to avoid.
    let mut last_ts = turn_started_ts(turn).unwrap_or_default();
    let mut assistant_cursor = 0usize;
    for item in items {
        if let Some(ts) = item_timestamp(item, &events, &mut assistant_cursor) {
            last_ts = ts;
        }
        if let Some(block) = completed_item_block(cwd, item) {
            rows.push((last_ts.clone(), order, block));
            order += 1;
        }
    }
    let mut emitted_shell_group = false;
    for event in &events {
        let block = match &event.kind {
            RolloutKind::Exec { .. } => {
                if emitted_shell_group {
                    continue;
                }
                emitted_shell_group = true;
                let group = events
                    .iter()
                    .copied()
                    .filter(|candidate| matches!(candidate.kind, RolloutKind::Exec { .. }))
                    .collect::<Vec<_>>();
                turn_shell_events_block(&group)
            }
            _ => event_block(event),
        };
        if let Some(block) = block {
            rows.push((event.ts.clone(), order, block));
            order += 1;
        }
    }
    // The timestamps are a fixed-width UTC format, so string order is time order.
    rows.sort_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));
    normalized_turn_blocks(rows.into_iter().map(|(_, _, block)| block).collect())
}

/// The turn's `startedAt` (unix seconds), formatted the same way the rollout's
/// own timestamps are so string-sorting the two together stays correct.
fn turn_started_ts(turn: &Value) -> Option<String> {
    let seconds = turn.get("startedAt").and_then(Value::as_i64)?;
    let moment = chrono::DateTime::from_timestamp(seconds, 0)?;
    Some(moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// The rollout events belonging to one turn. An event whose payload names its
/// own `turn_id` (every `Exec`, from `custom_tool_call`'s passthrough
/// metadata; `PatchApplied`, from `patch_apply_end`'s own field) is matched
/// directly against `turn.id` — the precise key the design called for.
/// Anything without one (assistant messages, reasoning — neither rollout form
/// carries a `turn_id` today) falls back to the turn's start/end window, and
/// an event with no attribution at all — no id, no window — is left out
/// rather than duplicated across every turn.
fn turn_events<'a>(turn: &Value, rollout: &'a Rollout) -> Vec<&'a RolloutEvent> {
    let turn_id = turn.get("id").and_then(Value::as_str);
    let started = turn.get("startedAt").and_then(Value::as_i64);
    let completed = turn.get("completedAt").and_then(Value::as_i64);
    rollout
        .events
        .iter()
        .filter(|event| match (event.turn_id.as_deref(), turn_id) {
            (Some(event_turn_id), Some(turn_id)) => event_turn_id == turn_id,
            _ => {
                let (Some(started), Some(completed)) = (started, completed) else {
                    return false;
                };
                unix_seconds(&event.ts)
                    .is_some_and(|seconds| seconds >= started && seconds <= completed)
            }
        })
        .collect()
}

fn unix_seconds(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|moment| moment.timestamp())
}

/// The last `agentMessage` item's text, in the server's own item order — used
/// instead of walking the merged/sorted blocks so a rollout-dated item never
/// changes which message counts as "last".
fn last_agent_message_text(items: &[Value]) -> Option<String> {
    items.iter().rev().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("agentMessage"))
            .then(|| item.get("text").and_then(Value::as_str))
            .flatten()
            .map(ToOwned::to_owned)
    })
}

/// When the rollout can date a server item. `cursor` walks the turn's assistant
/// messages so a repeated message text still anchors to its own occurrence.
fn item_timestamp(item: &Value, events: &[&RolloutEvent], cursor: &mut usize) -> Option<String> {
    match item.get("type").and_then(Value::as_str)? {
        "fileChange" => {
            let id = item.get("id").and_then(Value::as_str)?;
            events
                .iter()
                .find(|event| {
                    matches!(&event.kind, RolloutKind::PatchApplied { call_id } if call_id == id)
                })
                .map(|event| event.ts.clone())
        }
        "agentMessage" => {
            let text = item.get("text").and_then(Value::as_str)?;
            let offset = events.iter().skip(*cursor).position(|event| {
                matches!(&event.kind, RolloutKind::AssistantMessage { text: message } if message == text)
            })?;
            let index = *cursor + offset;
            *cursor = index + 1;
            Some(events[index].ts.clone())
        }
        _ => None,
    }
}

/// The block a rollout-only event becomes. Anchors produce nothing: the server
/// item they date is already in the transcript.
fn event_block(event: &RolloutEvent) -> Option<Block> {
    match &event.kind {
        RolloutKind::Exec {
            command,
            output,
            exit_code,
            duration_ms,
            ..
        } => {
            let suffix = exit_code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let duration = duration_ms
                .map(|duration| format!(" · {}", format_duration(duration)))
                .unwrap_or_default();
            Some(Block::new(
                if exit_code.unwrap_or(0) == 0 {
                    BlockKind::Tool
                } else {
                    BlockKind::Warning
                },
                format!("Shell · {}{suffix}{duration}", compact_command(command, 88)),
                // The rollout parser already strips the code-mode wrapper's
                // preamble (`rollout::command_result`) — that is the one place
                // that can see the `---N---` framing a multi-command script
                // uses, so it is also the only place that can strip each
                // command's own header without guessing at a `rfind`.
                collapse_output(&strip_ansi(output), 400),
            ))
        }
        RolloutKind::Reasoning { summary } => {
            Some(Block::new(BlockKind::Reasoning, "Thinking…", summary))
        }
        RolloutKind::PatchApplied { .. } | RolloutKind::AssistantMessage { .. } => None,
    }
}

fn shell_events_block(events: &[&RolloutEvent]) -> Option<Block> {
    if events.is_empty() {
        return None;
    }

    let results = events
        .iter()
        .filter_map(|event| {
            let RolloutKind::Exec {
                exit_code,
                duration_ms: event_duration,
                ..
            } = &event.kind
            else {
                return None;
            };
            Some(ShellResult {
                block: event_block(event)?,
                exit_code: *exit_code,
                duration_ms: *event_duration,
            })
        })
        .collect::<Vec<_>>();
    (results.len() == events.len()).then(|| shell_results_block(results))
}

fn turn_shell_events_block(events: &[&RolloutEvent]) -> Option<Block> {
    let mut group_durations = HashMap::<&str, u64>::new();
    for event in events {
        if let RolloutKind::Exec {
            group_id,
            duration_ms: Some(duration_ms),
            ..
        } = &event.kind
        {
            group_durations
                .entry(group_id.as_str())
                .and_modify(|duration| *duration = (*duration).max(*duration_ms))
                .or_insert(*duration_ms);
        }
    }
    let duration_ms =
        (!group_durations.is_empty()).then(|| group_durations.values().copied().sum());
    let mut block = shell_events_block(events)?;
    if events.len() > 1 {
        let results = events
            .iter()
            .filter_map(|event| {
                let RolloutKind::Exec {
                    exit_code,
                    duration_ms,
                    ..
                } = &event.kind
                else {
                    return None;
                };
                Some(ShellResult {
                    block: event_block(event)?,
                    exit_code: *exit_code,
                    duration_ms: *duration_ms,
                })
            })
            .collect::<Vec<_>>();
        block = shell_results_block_with_duration(results, duration_ms);
    }
    Some(block)
}

fn shell_results_block(results: Vec<ShellResult>) -> Block {
    let duration_ms = results.iter().filter_map(|result| result.duration_ms).max();
    shell_results_block_with_duration(results, duration_ms)
}

fn shell_results_block_with_duration(results: Vec<ShellResult>, duration_ms: Option<u64>) -> Block {
    assert!(!results.is_empty(), "shell group needs at least one result");

    let failed = results
        .iter()
        .filter(|result| result.exit_code.is_some_and(|code| code != 0))
        .count();
    let status = if failed > 0 {
        format!("{failed} failed")
    } else {
        "completed".to_owned()
    };
    let duration = duration_ms
        .map(|duration| format!(" · {}", format_duration(duration)))
        .unwrap_or_default();
    let count = results.len();
    let noun = if count == 1 { "command" } else { "commands" };
    let children = results
        .into_iter()
        .map(|result| result.block)
        .collect::<Vec<_>>();

    Block::shell_group(
        if failed > 0 {
            BlockKind::Warning
        } else {
            BlockKind::Tool
        },
        format!("Shell · {count} {noun} · {status}{duration}"),
        children,
    )
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

/// Heading for a `fileChange` item: the action and the file it touched. A batch
/// is counted instead, and the body then
/// names each file itself.
fn file_changes_title(cwd: &str, changes: &[Value]) -> String {
    match changes {
        [single] => format!(
            "{}({})",
            change_verb(single),
            display_path(cwd, change_path(single))
        ),
        _ => format!("Update({} files)", changes.len()),
    }
}

/// The first row summarises the whole batch — the renderer hangs it off the
/// heading under a `⎿`. After it come the patches, with the framing git wraps a
/// diff in dropped: the renderer needs the `@@` headers for line numbers and
/// nothing above them. A batch gets a heading row per file; a lone file is
/// already named by [`file_changes_title`].
fn file_changes_body(cwd: &str, changes: &[Value]) -> String {
    let (additions, deletions) = changes
        .iter()
        .map(|change| diff_stats(change_diff(change)))
        .fold((0, 0), |total, stats| {
            (total.0 + stats.0, total.1 + stats.1)
        });
    let mut rows = vec![format!(
        "Added {additions} {}, removed {deletions} {}",
        plural(additions, "line"),
        plural(deletions, "line")
    )];
    for change in changes {
        if changes.len() > 1 {
            rows.push(format!(
                "{}({})",
                change_verb(change),
                display_path(cwd, change_path(change))
            ));
        }
        rows.extend(
            diff_rows(change_diff(change))
                .into_iter()
                .map(str::to_owned),
        );
    }
    rows.join("\n")
}

fn change_path(change: &Value) -> &str {
    change
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn change_diff(change: &Value) -> &str {
    change
        .get("diff")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn change_verb(change: &Value) -> &'static str {
    match change
        .get("kind")
        .and_then(|kind| kind.get("type"))
        .and_then(Value::as_str)
    {
        Some("add") => "Add",
        Some("delete") => "Delete",
        _ => "Update",
    }
}

/// A unified diff from its first hunk header on. Everything above that — `diff
/// --git`, `index`, the `---`/`+++` pair — is framing, and cutting at the header
/// rather than matching those prefixes keeps a removed `--- ` row of real content
/// out of the crossfire. Patches without hunk headers are passed through whole.
fn diff_rows(diff: &str) -> Vec<&str> {
    let rows = diff.lines().collect::<Vec<_>>();
    match rows.iter().position(|row| row.starts_with("@@")) {
        Some(start) => rows[start..].to_vec(),
        None => rows,
    }
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let rows = diff_rows(diff);
    (
        rows.iter().filter(|row| row.starts_with('+')).count(),
        rows.iter().filter(|row| row.starts_with('-')).count(),
    )
}

/// Absolute paths are what the app-server reports, but the session's own
/// directory is noise in front of every one of them. Whichever separator the path
/// arrived with is kept, so a Windows path still reads as `src\file.rs`.
fn display_path(cwd: &str, path: &str) -> String {
    // Windows is case-insensitive and takes either separator, so both are folded
    // for the comparison without touching what gets displayed.
    let fold = |byte: u8| match byte {
        b'\\' => b'/',
        other => other.to_ascii_lowercase(),
    };
    let root = cwd.trim_end_matches(['/', '\\']).as_bytes();
    let inside = !root.is_empty()
        && path.len() > root.len()
        && path.as_bytes()[..root.len()]
            .iter()
            .zip(root)
            .all(|(left, right)| fold(*left) == fold(*right))
        && matches!(path.as_bytes()[root.len()], b'/' | b'\\');
    match inside {
        true => path[root.len() + 1..].to_owned(),
        false => path.to_owned(),
    }
}

fn plural(count: usize, noun: &str) -> String {
    match count {
        1 => noun.to_owned(),
        _ => format!("{noun}s"),
    }
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

/// The tiers a model supports, for the renderer to lay out and colour.
fn effort_slider(model: &ModelInfo, selected: usize) -> EffortSlider {
    EffortSlider {
        efforts: model
            .efforts
            .iter()
            .map(|effort| effort.id.clone())
            .collect(),
        selected: selected.min(model.efforts.len().saturating_sub(1)),
    }
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

/// Removes ANSI escape sequences from captured command output. Codex runs the
/// shell on a pty, so colours, cursor moves and title sets arrive mixed into the
/// text; left in, they break the renderer's column arithmetic — which decides
/// where a row wraps, and so how much of the row budget it spends — and paint as
/// garbage.
fn strip_ansi(output: &str) -> String {
    let mut clean = String::with_capacity(output.len());
    let mut chars = output.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            clean.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one byte in `@`..=`~` ends it.
            Some('[') => {
                for ch in chars.by_ref() {
                    if matches!(ch, '\x40'..='\x7e') {
                        break;
                    }
                }
            }
            // OSC: a string ending at BEL, or at ST (`ESC \`).
            Some(']') => loop {
                match chars.next() {
                    None | Some('\x07') => break,
                    Some('\x1b') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {}
                }
            },
            // Anything else is a two-character sequence; both are dropped.
            _ => {}
        }
    }
    clean
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

fn read_vibe_mode() -> VibeMode {
    read_vibe_config_value("vibe_mode")
        .map(|value| match value.as_str() {
            "super_vibe" => VibeMode::SuperVibe,
            "normal" => VibeMode::Normal,
            _ => VibeMode::Vibe,
        })
        .unwrap_or_default()
}

fn read_conversation_view() -> ConversationView {
    match read_vibe_config_value("conversation_view").as_deref() {
        Some("chat") => ConversationView::Chat,
        _ => ConversationView::List,
    }
}

#[allow(dead_code)]
fn read_response_length() -> ResponseLength {
    read_vibe_config_value("model_verbosity")
        .map(|value| match value.as_str() {
            "medium" => ResponseLength::Normal,
            "high" => ResponseLength::Detailed,
            _ => ResponseLength::Short,
        })
        .unwrap_or_default()
}

fn read_vibe_config_value(key: &str) -> Option<String> {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .and_then(|config| config.lines().find_map(|line| {
            let (found, value) = line.split('#').next()?.split_once('=')?;
            (found.trim() == key).then(|| value.trim().trim_matches(['\"', '\'']).to_ascii_lowercase())
        }))
}

#[allow(dead_code)]
fn read_shell_display_mode() -> ShellDisplayMode {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .and_then(|config| parse_shell_display_mode(&config))
        .unwrap_or_default()
}

#[allow(dead_code)]
fn read_diff_display_mode() -> DiffDisplayMode {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .and_then(|config| parse_diff_display_mode(&config))
        .unwrap_or_default()
}

fn read_status_line_settings() -> StatusLineSettings {
    codex_home()
        .and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
        .map(|config| {
            let mut settings = StatusLineSettings::default();
            for field in StatusLineField::ALL {
                if let Some(enabled) = parse_status_line_field(&config, field) {
                    settings.0[field.index()] = enabled;
                }
            }
            settings
        })
        .unwrap_or_default()
}

#[allow(dead_code)]
fn parse_shell_display_mode(config: &str) -> Option<ShellDisplayMode> {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "shell_display_mode").then(|| ShellDisplayMode::from_config_value(value))
        })
        .flatten()
}

#[allow(dead_code)]
fn parse_diff_display_mode(config: &str) -> Option<DiffDisplayMode> {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "diff_display_mode").then(|| DiffDisplayMode::from_config_value(value))
        })
        .flatten()
}

fn parse_status_line_field(config: &str, field: StatusLineField) -> Option<bool> {
    config
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| line.split('#').next())
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == field.config_key()).then(|| {
                match value
                    .trim()
                    .trim_matches(['\"', '\''])
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            })
        })
        .flatten()
}

pub(crate) fn codex_home() -> Option<PathBuf> {
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

    /// The old summary said only `± <absolute path>  +17 -8`; a reviewer needs the
    /// patch itself, so the block now carries the hunks and drops git's framing.
    #[test]
    fn file_change_block_keeps_the_patch_and_relativizes_the_path() {
        let changes = vec![json!({
            "path": r"C:\Source\DevezVibe\src\main.rs",
            "kind": { "type": "update" },
            "diff": "diff --git a/src/main.rs b/src/main.rs\nindex 111..222 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -83,3 +83,4 @@\n context\n-let old = 1;\n+let new = 2;\n+let extra = 3;\n"
        })];
        let cwd = r"C:\Source\DevezVibe";

        assert_eq!(file_changes_title(cwd, &changes), r"Update(src\main.rs)");
        assert_eq!(
            file_changes_body(cwd, &changes),
            "Added 2 lines, removed 1 line\n@@ -83,3 +83,4 @@\n context\n-let old = 1;\n+let new = 2;\n+let extra = 3;"
        );
    }

    /// A patch that removes a line of content starting with `---` used to have that
    /// row counted as git framing and dropped from the diff.
    #[test]
    fn diff_rows_cut_at_the_hunk_header_not_at_dashed_prefixes() {
        let diff = "--- a/notes.md\n+++ b/notes.md\n@@ -1,2 +1,1 @@\n title\n---\n";
        assert_eq!(diff_rows(diff), ["@@ -1,2 +1,1 @@", " title", "---"]);
        assert_eq!(diff_stats(diff), (0, 1));
    }

    #[test]
    fn display_path_leaves_paths_outside_the_session_alone() {
        assert_eq!(
            display_path(r"C:\Source\DevezVibe", r"C:\Other\file.rs"),
            r"C:\Other\file.rs"
        );
        // Case and separator both fold for the comparison; the display keeps the
        // separator the path arrived with.
        assert_eq!(
            display_path(r"C:\Source\DevezVibe", r"c:/source/devezvibe\src\a.rs"),
            r"src\a.rs"
        );
    }

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
    fn permission_profile_is_fixed_to_full_access() {
        let mut state = test_state();

        state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(state.permission_mode(), PermissionMode::FullAccess);
        assert_eq!(state.permission_profile(), ":danger-full-access");
    }

    #[test]
    fn response_length_cycles_from_short_to_normal_to_detailed() {
        let mut state = test_state();

        assert_eq!(state.response_length_label(), "Short");
        assert_eq!(state.model_verbosity(), "low");
        state.cycle_response_length();
        assert_eq!(state.response_length_label(), "Normal");
        assert_eq!(state.model_verbosity(), "medium");
        state.cycle_response_length();
        assert_eq!(state.response_length_label(), "Detailed");
        assert_eq!(state.model_verbosity(), "high");
    }

    #[test]
    fn transcript_display_defaults_to_hidden_shell_and_diff() {
        assert_eq!(ShellDisplayMode::default(), ShellDisplayMode::Hide);
        assert_eq!(DiffDisplayMode::default(), DiffDisplayMode::Hide);
    }

    #[test]
    fn vibe_mode_defaults_to_short_collapsed_output() {
        let state = test_state();

        assert_eq!(state.vibe_mode_label(), "Vibe");
        assert_eq!(state.response_length_label(), "Short");
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Collapse);
        assert_eq!(state.diff_display_mode(), DiffDisplayMode::Collapse);
    }

    #[test]
    fn slash_display_setting_switches_vibe_mode_to_custom() {
        let mut state = test_state();

        state.run_slash_command("/shell hide");

        assert_eq!(state.vibe_mode_label(), "Custom");
        assert_eq!(state.shell_display_mode(), ShellDisplayMode::Hide);
    }

    #[test]
    fn vibe_mode_picker_previews_changes_and_escape_restores_them() {
        let mut state = test_state();
        state.run_slash_command("/vibemode");

        state.handle_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.response_length_label(), "Normal");
        assert_eq!(state.vibe_mode_label(), "Custom");

        state.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(state.response_length_label(), "Short");
        assert_eq!(state.vibe_mode_label(), "Vibe");
    }

    #[test]
    fn shifted_arrows_change_model_and_effort_without_wrapping() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6 Terra", false));
        let shift = KeyModifiers::SHIFT;

        state.handle_key(KeyEvent::new(KeyCode::Up, shift));
        assert_eq!(state.selected_model_name(), "gpt-5.6-sol");

        state.handle_key(KeyEvent::new(KeyCode::Down, shift));
        assert_eq!(state.selected_model_name(), "gpt-5.6-terra");
        assert_eq!(state.selected_effort(), "high");
        assert!(state.committed.is_empty());

        state.handle_key(KeyEvent::new(KeyCode::Down, shift));
        assert_eq!(state.selected_model_name(), "gpt-5.6-terra");

        state.handle_key(KeyEvent::new(KeyCode::Left, shift));
        assert_eq!(state.selected_effort(), "medium");

        state.handle_key(KeyEvent::new(KeyCode::Right, shift));
        assert_eq!(state.selected_effort(), "high");
        for _ in 0..8 {
            state.handle_key(KeyEvent::new(KeyCode::Right, shift));
        }
        assert_eq!(state.selected_effort(), "ultra");

        state.handle_key(KeyEvent::new(KeyCode::Right, shift));
        assert_eq!(state.selected_effort(), "ultra");
        assert!(state.committed.is_empty());
    }

    #[test]
    fn shifted_model_and_effort_changes_announce_the_next_request_while_busy() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6 Terra", false));
        state.busy = true;

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(
            state.view().composer_notice.as_deref(),
            Some("Applies to the next request")
        );

        state.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(
            state.view().composer_notice.as_deref(),
            Some("Applies to the next request")
        );

        state.busy = false;
        state.composer_notice = None;
        state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(state.view().composer_notice, None);
    }

    #[test]
    fn esc_while_turn_is_starting_defers_one_interrupt_until_started() {
        let mut state = test_state();
        state.busy = true;
        state.turn_started_at = Some(Instant::now());

        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Tick(true)
        ));
        assert!(
            state
                .activity()
                .is_some_and(|activity| activity == "X Interrupted")
        );
        assert_eq!(state.take_pending_interrupt(), None);

        state.set_turn_started("turn-1".to_owned());
        assert_eq!(state.take_pending_interrupt().as_deref(), Some("turn-1"));
        assert_eq!(state.take_pending_interrupt(), None);
    }

    #[test]
    fn failed_start_clears_a_deferred_interrupt() {
        let mut state = test_state();
        state.busy = true;

        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        state.set_request_failed("start failed");
        state.set_turn_started("next-turn".to_owned());

        assert_eq!(state.take_pending_interrupt(), None);
    }

    #[test]
    fn command_block_identity_survives_active_to_completed_transition() {
        let mut state = test_state();
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "aggregatedOutput": "one"
        }));
        let active_id = state.active["cmd-1"].block.id();

        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "status": "completed",
            "exitCode": 0,
            "durationMs": 12,
            "aggregatedOutput": "one"
        }));

        assert_eq!(state.committed.last().unwrap().id(), active_id);
    }

    #[test]
    fn active_shell_commands_do_not_duplicate_the_committed_anchor() {
        let mut state = test_state();
        for id in ["cmd-1", "cmd-2"] {
            state.start_item(&json!({
                "id": id,
                "type": "commandExecution",
                "command": "rg TODO"
            }));
        }

        let titles = state
            .view()
            .live_blocks
            .into_iter()
            .map(|block| block.title)
            .collect::<Vec<_>>();

        assert!(titles.is_empty());
        assert_eq!(state.committed.len(), 1);
        assert_eq!(state.committed[0].title, "Running 2 shell commands");
    }

    #[test]
    fn shell_output_before_started_never_enters_the_live_transcript() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;

        state.handle_notification(
            "item/commandExecution/outputDelta",
            &json!({ "itemId": "cmd-1", "delta": "early output" }),
        );

        assert!(state.view().live_blocks.is_empty());
        let anchor = state.drain_committed();
        assert_eq!(anchor.len(), 1);
        assert_eq!(anchor[0].title, "Running 1 shell command");

        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO"
        }));
        assert_eq!(state.active["cmd-1"].block.body, "early output");
        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "status": "completed",
            "exitCode": 0
        }));

        let completed = state.drain_committed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].children()[0].body, "early output");
    }

    #[test]
    fn live_shell_is_hidden_by_batch_membership_not_its_running_title() {
        let mut state = test_state();
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO"
        }));
        state.active.get_mut("cmd-1").unwrap().block.title = "Running shell command".to_owned();

        assert!(state.view().live_blocks.is_empty());
    }

    #[test]
    fn hide_filters_an_unbatched_running_shell_status_before_rendering() {
        let mut state = test_state();
        state.shell_display_mode = ShellDisplayMode::Hide;
        state.ensure_active("transient", BlockKind::System, "Running Shell Command");

        assert!(state.view().live_blocks.is_empty());

        let mut state = test_state();
        state.shell_display_mode = ShellDisplayMode::Hide;
        state
            .ensure_active("tool-output", BlockKind::Tool, "Command")
            .block
            .body = "Running Shell Command".to_owned();
        assert!(state.view().live_blocks.is_empty());
    }

    #[test]
    fn hide_never_hands_tool_rows_to_the_renderer() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Hide;
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO"
        }));

        assert!(state.drain_committed().is_empty());

        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "status": "completed",
            "exitCode": 0
        }));

        assert!(state.drain_committed().is_empty());

        state.start_item(&json!({
            "id": "search-1",
            "type": "webSearch",
            "query": "rust ownership"
        }));

        assert!(state.view().live_blocks.is_empty());

        state.complete_item(&json!({
            "id": "search-1",
            "type": "webSearch",
            "query": "rust ownership"
        }));

        assert!(state.drain_committed().is_empty());

        state
            .ensure_active("dynamic-tool", BlockKind::Tool, "Tool · lookup");
        state.ensure_active("agent-tool", BlockKind::Tool, "Agent");
        assert!(state.view().live_blocks.is_empty());

        state.start_item(&json!({
            "id": "node-repl-1",
            "type": "mcpToolCall",
            "server": "node_repl",
            "tool": "js",
            "arguments": { "code": "1 + 1" }
        }));

        assert!(state.view().live_blocks.is_empty());

        state.complete_item(&json!({
            "id": "node-repl-1",
            "type": "mcpToolCall",
            "server": "node_repl",
            "tool": "js",
            "arguments": { "code": "1 + 1" },
            "result": { "content": [{ "type": "text", "text": "2" }] }
        }));

        assert!(state.drain_committed().is_empty());
    }

    #[test]
    fn completed_shell_group_reuses_its_running_anchor_id() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO"
        }));

        let anchors = state.drain_committed();
        assert_eq!(anchors.len(), 1);
        let anchor_id = anchors[0].id();
        assert!(state.view().live_blocks.is_empty());

        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "status": "completed",
            "exitCode": 0,
            "aggregatedOutput": "done"
        }));

        let completed = state.drain_committed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id(), anchor_id);
        assert_eq!(completed[0].title, "Shell · 1 command · completed");
    }

    #[test]
    fn consecutive_live_thinking_keeps_only_the_latest() {
        let mut state = test_state();
        for (id, summary) in [("r1", "first"), ("r2", "latest")] {
            state.complete_item(&json!({
                "id": id,
                "type": "reasoning",
                "summary": [summary]
            }));
        }

        assert_eq!(state.committed.len(), 1);
        assert_eq!(state.committed[0].title, "Thinking…");
        assert_eq!(state.committed[0].body, "latest");
    }

    #[test]
    fn empty_thinking_never_enters_the_live_or_committed_transcript() {
        let mut state = test_state();
        state.ensure_active("reasoning", BlockKind::Reasoning, "Thinking…");
        assert!(state.view().live_blocks.is_empty());

        state.complete_item(&json!({
            "id": "reasoning",
            "type": "reasoning",
            "summary": []
        }));
        assert!(state.committed.is_empty());
    }

    #[test]
    fn shell_between_thinking_blocks_preserves_both() {
        let mut state = test_state();
        state.complete_item(&json!({
            "id": "r1",
            "type": "reasoning",
            "summary": ["first"]
        }));
        state.complete_item(&json!({
            "id": "cmd",
            "type": "commandExecution",
            "command": "pwd",
            "status": "completed",
            "exitCode": 0
        }));
        state.complete_item(&json!({
            "id": "r2",
            "type": "reasoning",
            "summary": ["second"]
        }));

        let thinking = state
            .committed
            .iter()
            .filter(|block| block.title == "Thinking…")
            .count();
        assert_eq!(thinking, 2);
    }

    #[test]
    fn resumed_turn_keeps_latest_consecutive_thinking_per_run() {
        let blocks = normalized_turn_blocks(vec![
            Block::new(BlockKind::Reasoning, "Thinking…", "first"),
            Block::new(BlockKind::Reasoning, "Thinking…", "latest"),
            Block::new(BlockKind::Tool, "Shell · pwd", ""),
            Block::new(BlockKind::Reasoning, "Thinking…", "after shell"),
        ]);

        let bodies = blocks
            .iter()
            .filter(|block| block.title == "Thinking…")
            .map(|block| block.body.as_str())
            .collect::<Vec<_>>();
        assert_eq!(bodies, ["latest", "after shell"]);
    }

    #[test]
    fn web_search_replays_and_repeated_queries_are_deduplicated() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;
        state.set_turn_started("turn-1".to_owned());

        for id in ["search-1", "search-2"] {
            state.start_item(&json!({
                "id": id,
                "type": "webSearch",
                "query": "rust async"
            }));
        }
        let live = state.view().live_blocks;
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].title, "Web search · rust async");

        let first = json!({
            "id": "search-1",
            "type": "webSearch",
            "query": "rust async"
        });
        state.complete_item(&first);
        state.complete_item(&first);
        state.complete_item(&json!({
            "id": "search-2",
            "type": "webSearch",
            "query": "rust async"
        }));
        state.complete_item(&json!({
            "id": "search-3",
            "type": "webSearch",
            "query": "rust channels"
        }));

        let titles = state
            .drain_committed()
            .into_iter()
            .map(|block| block.title)
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            ["Web search · rust async", "Web search · rust channels"]
        );
    }

    #[test]
    fn operation_deduplication_resets_for_each_turn() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;
        for (turn, id) in [("turn-1", "search-1"), ("turn-2", "search-2")] {
            state.set_turn_started(turn.to_owned());
            state.complete_item(&json!({
                "id": id,
                "type": "webSearch",
                "query": "same query"
            }));
        }

        assert_eq!(state.drain_committed().len(), 2);
    }

    #[test]
    fn sequential_file_changes_replace_one_turn_group() {
        let mut state = test_state();
        state.show_welcome = false;
        state.cwd = r"C:\Source\DevezVibe".to_owned();
        state.set_turn_started("turn-1".to_owned());

        let change = |id: &str, old: &str, new: &str| {
            json!({
                "id": id,
                "type": "fileChange",
                "changes": [{
                    "path": r"C:\Source\DevezVibe\src\state.rs",
                    "kind": { "type": "update" },
                    "diff": format!("@@ -1 +1 @@\n-{old}\n+{new}")
                }]
            })
        };

        state.complete_item(&change("patch-1", "one", "two"));
        let first = state.drain_committed();
        assert_eq!(first.len(), 1);
        let group_id = first[0].id();
        assert_eq!(first[0].title, r"Update(src\state.rs)");
        assert_eq!(first[0].children().len(), 1);

        state.complete_item(&change("patch-2", "two", "three"));
        let second = state.drain_committed();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id(), group_id);
        assert_eq!(second[0].title, r"Update(src\state.rs)");
        assert_eq!(second[0].children().len(), 2);
    }

    #[test]
    fn resumed_file_changes_become_one_turn_group() {
        let blocks = normalized_turn_blocks(vec![
            Block::new(
                BlockKind::FileChange,
                "Update(src/state.rs)",
                "Added 1 line, removed 0 lines\n@@ -1,0 +1 @@\n+one",
            ),
            Block::new(BlockKind::Reasoning, "Thinking…", "continuing"),
            Block::new(
                BlockKind::FileChange,
                "Update(src/state.rs)",
                "Added 1 line, removed 1 line\n@@ -1 +1 @@\n-one\n+two",
            ),
        ]);

        let changes = blocks
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::FileChange))
            .collect::<Vec<_>>();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].children().len(), 2);
        assert_eq!(changes[0].title, "Update(src/state.rs)");
    }

    #[test]
    fn resumed_turn_deduplicates_identical_operation_cards() {
        let blocks = normalized_turn_blocks(vec![
            Block::new(BlockKind::Tool, "Web search · rust", ""),
            Block::new(BlockKind::Tool, "Web search · rust", ""),
            Block::new(BlockKind::Tool, "MCP · docs › search", "same result"),
            Block::new(BlockKind::Tool, "MCP · docs › search", "same result"),
            Block::new(BlockKind::Tool, "MCP · docs › search", "different result"),
            Block::new(BlockKind::FileChange, "Update(src/a.rs)", "+one"),
            Block::new(BlockKind::FileChange, "Update(src/a.rs)", "+one"),
        ]);

        assert_eq!(blocks.len(), 4);
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block.title.starts_with("Web search"))
                .count(),
            1
        );
    }

    #[test]
    fn duplicate_plan_notifications_replace_the_fixed_summary() {
        let mut state = test_state();
        state.show_welcome = false;
        state.set_turn_started("turn-1".to_owned());
        let plan = json!({
            "explanation": "same",
            "plan": [{ "step": "check", "status": "inProgress" }]
        });

        state.handle_notification("turn/plan/updated", &plan);
        state.handle_notification("turn/plan/updated", &plan);
        state.complete_item(&json!({
            "id": "compact-1",
            "type": "contextCompaction"
        }));
        state.handle_notification("thread/compacted", &json!({}));

        assert_eq!(
            state
                .committed
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::Plan))
                .count(),
            0
        );
        assert_eq!(state.plan_summary.as_ref().map(|summary| summary.steps.len()), Some(1));
        assert_eq!(
            state
                .committed
                .iter()
                .filter(|block| block.title == "Context compacted")
                .count(),
            1
        );
    }

    #[test]
    fn live_overlapping_shells_commit_as_one_group() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO"
        }));
        state.start_item(&json!({
            "id": "cmd-2",
            "type": "commandExecution",
            "command": "git status --short"
        }));
        let anchors = state.drain_committed();
        assert_eq!(anchors.len(), 1);

        state.complete_item(&json!({
            "id": "cmd-2",
            "type": "commandExecution",
            "command": "git status --short",
            "status": "completed",
            "exitCode": 1,
            "durationMs": 12,
            "aggregatedOutput": "failed"
        }));
        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": "rg TODO",
            "status": "completed",
            "exitCode": 0,
            "durationMs": 18,
            "aggregatedOutput": "match"
        }));

        let completed = state.drain_committed();
        assert_eq!(completed.len(), 1);
        let group = &completed[0];
        assert_eq!(group.title, "Shell · 2 commands · 1 failed · 18ms");
        assert!(matches!(group.kind, BlockKind::Warning));
        assert_eq!(group.children().len(), 2);
        assert!(group.children()[0].title.starts_with("Shell · rg TODO"));
        assert!(
            group.children()[1]
                .title
                .starts_with("Shell · git status --short")
        );
    }

    #[test]
    fn live_single_shell_hides_its_command_in_the_summary() {
        let mut state = test_state();
        let command =
            r#"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -Command Get-Content"#;
        state.start_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": command
        }));
        state.complete_item(&json!({
            "id": "cmd-1",
            "type": "commandExecution",
            "command": command,
            "status": "completed",
            "exitCode": 0,
            "durationMs": 670,
            "aggregatedOutput": "contents"
        }));

        let shell = state.committed.last().expect("completed shell");
        assert_eq!(shell.title, "Shell · 1 command · completed · 670ms");
        assert_eq!(shell.children().len(), 1);
        assert!(shell.children()[0].title.contains("powershell.exe"));
        assert_eq!(shell.children()[0].body, "contents");
    }

    #[test]
    fn sequential_shells_update_one_turn_group() {
        let mut state = test_state();
        state.show_welcome = false;
        state.shell_display_mode = ShellDisplayMode::Collapse;
        state.set_turn_started("turn-1".to_owned());
        let mut group_id = None;
        for (id, command, duration_ms) in [
            ("cmd-1", "rg TODO", 700),
            ("cmd-2", "git status --short", 800),
        ] {
            state.start_item(&json!({
                "id": id,
                "type": "commandExecution",
                "command": command
            }));
            let anchors = state.drain_committed();
            assert_eq!(anchors.len(), 1);
            if let Some(group_id) = group_id {
                assert_eq!(anchors[0].id(), group_id);
            }
            group_id = Some(anchors[0].id());
            state.complete_item(&json!({
                "id": id,
                "type": "commandExecution",
                "command": command,
                "status": "completed",
                "exitCode": if id == "cmd-2" { 1 } else { 0 },
                "durationMs": duration_ms
            }));
            let completed = state.drain_committed();
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].id(), group_id.expect("group id"));
        }

        let completed = state
            .turn_shell_anchor
            .as_ref()
            .expect("completed turn shell group");
        assert_eq!(completed.title, "Shell · 2 commands · 1 failed · 1.5s");
        assert_eq!(completed.children().len(), 2);
        assert!(completed.children()[0].title.contains("rg TODO"));
        assert!(completed.children()[1].title.contains("git status --short"));
    }

    /// A turn covering 15:08:28–15:12:59 UTC on 2026-07-25, matching the
    /// timestamps the rollout literals below use.
    fn history_thread() -> Value {
        json!({
            "turns": [{
                "id": "turn-1",
                "startedAt": 1_784_992_108_i64,
                "completedAt": 1_784_992_379_i64,
                "items": [
                    { "type": "agentMessage", "id": "item-1", "text": "확인해봤습니다" },
                    { "type": "fileChange", "id": "exec-abc", "changes": [] },
                    { "type": "agentMessage", "id": "item-2", "text": "고쳤습니다" }
                ]
            }]
        })
    }

    #[test]
    fn resumed_shell_runs_land_between_the_messages_they_ran_under() {
        let mut state = test_state();
        // 15:08:33 message, 15:08:36 shell run, 15:09:40 patch, 15:09:58 message.
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:33.387Z","type":"event_msg","payload":{"type":"agent_message","message":"확인해봤습니다"}}
{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Script completed\nWall time 1.6 seconds\nOutput:\n"},{"type":"input_text","text":"Exit code: 0\nWall time: 0.5 seconds\nOutput:\nok\n"}]}}
{"timestamp":"2026-07-25T15:09:40.539Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"exec-abc"}}
{"timestamp":"2026-07-25T15:09:58.000Z","type":"event_msg","payload":{"type":"agent_message","message":"고쳤습니다"}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        let titles = state
            .committed
            .iter()
            .map(|block| block.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles[0], "Codex");
        assert_eq!(titles[1], "Shell · 1 command · completed · 1.6s");
        assert_eq!(titles[3], "Codex");
        assert!(matches!(state.committed[1].kind, BlockKind::Tool));
        assert_eq!(state.committed[1].children().len(), 1);
        assert_eq!(
            state.committed[1].children()[0].title,
            "Shell · cargo test · exit 0 · 1.6s"
        );
        assert_eq!(state.committed[1].children()[0].body, "ok");
        // The file change sorts by its `patch_apply_end` time: after the shell run
        // at 15:08:36, before the message at 15:09:58.
        assert!(matches!(state.committed[2].kind, BlockKind::FileChange));
    }

    #[test]
    fn resumed_multi_command_exec_becomes_one_shell_group() {
        let mut state = test_state();
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_pair","input":"const r = await Promise.all([tools.shell_command({command:\"rg TODO\"}),tools.shell_command({command:\"git status --short\"})]);","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_pair","output":[{"type":"input_text","text":"Script completed\nWall time 4.1 seconds\nOutput:\n"},{"type":"input_text","text":"---0---Exit code: 0\nOutput:\nmatch\n---1---Exit code: 1\nOutput:\nfailed\n"}]}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        let shell = state
            .committed
            .iter()
            .find(|block| block.title.starts_with("Shell · 2 commands"))
            .expect("grouped shell block");
        assert_eq!(shell.title, "Shell · 2 commands · 1 failed · 4.1s");
        assert!(matches!(shell.kind, BlockKind::Warning));
        assert_eq!(shell.children().len(), 2);
        assert_eq!(shell.children()[0].title, "Shell · rg TODO · exit 0 · 4.1s");
        assert_eq!(
            shell.children()[1].title,
            "Shell · git status --short · exit 1 · 4.1s"
        );
    }

    #[test]
    fn resumed_sequential_execs_become_one_turn_shell_group() {
        let mut state = test_state();
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"rg TODO\"});","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}}}
{"timestamp":"2026-07-25T15:08:37.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Wall time 0.7 seconds\n"},{"type":"input_text","text":"Exit code: 0\nOutput:\nmatch\n"}]}}
{"timestamp":"2026-07-25T15:08:38.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_two","input":"await tools.shell_command({\"command\":\"git status --short\"});","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}}}
{"timestamp":"2026-07-25T15:08:39.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_two","output":[{"type":"input_text","text":"Wall time 0.8 seconds\n"},{"type":"input_text","text":"Exit code: 1\nOutput:\nfailed\n"}]}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        let shells = state
            .committed
            .iter()
            .filter(|block| block.title.starts_with("Shell ·"))
            .collect::<Vec<_>>();
        assert_eq!(shells.len(), 1);
        assert_eq!(shells[0].title, "Shell · 2 commands · 1 failed · 1.5s");
        assert_eq!(shells[0].children().len(), 2);
    }

    #[test]
    fn a_failed_shell_run_is_resumed_as_a_warning() {
        let mut state = test_state();
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});"}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Wall time 2.0 seconds\n"},{"type":"input_text","text":"Exit code: 101\nOutput:\nfailed\n"}]}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        // Nothing in this rollout anchors any server item (no matching
        // `AssistantMessage`/`PatchApplied`), so all three items inherit the
        // turn's start time and the shell run — the only thing with a real
        // timestamp — sorts after them. Asserting the position (not just
        // finding the block by title) is what catches an order regression.
        assert_eq!(state.committed.len(), 4);
        let bash = &state.committed[3];
        assert_eq!(bash.title, "Shell · 1 command · 1 failed · 2.0s");
        assert!(matches!(bash.kind, BlockKind::Warning));
        assert_eq!(bash.children().len(), 1);
        assert_eq!(
            bash.children()[0].title,
            "Shell · cargo test · exit 101 · 2.0s"
        );
    }

    #[test]
    fn history_without_a_rollout_keeps_the_server_item_order() {
        let mut state = test_state();

        state.load_history(&history_thread(), None);

        let titles = state
            .committed
            .iter()
            .map(|block| block.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles.len(), 3);
        assert_eq!(titles[0], "Codex");
        assert_eq!(titles[1], "Update(0 files)");
        assert_eq!(titles[2], "Codex");
    }

    #[test]
    fn rollout_events_outside_the_turn_window_are_left_out() {
        let mut state = test_state();
        // 15:20:00 is past this turn's 15:12:59 end.
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:20:00.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_late","input":"await tools.shell_command({\"command\":\"git status\"});"}}"#,
        );

        state.load_history(&history_thread(), Some(&rollout));

        assert!(
            !state
                .committed
                .iter()
                .any(|block| block.title.starts_with("Shell ·"))
        );
    }

    #[test]
    fn turn_id_attribution_overrides_an_overlapping_time_window() {
        // Two turns with identical windows (a contrived worst case): the exec
        // event's own `turn_id` must decide which turn it lands in, not the
        // time-window fallback that used to be the only signal available.
        let mut state = test_state();
        let thread = json!({
            "turns": [
                {
                    "id": "turn-1",
                    "startedAt": 1_784_992_100_i64,
                    "completedAt": 1_784_992_500_i64,
                    "items": [{ "type": "agentMessage", "id": "item-1", "text": "first turn" }]
                },
                {
                    "id": "turn-2",
                    "startedAt": 1_784_992_100_i64,
                    "completedAt": 1_784_992_500_i64,
                    "items": [{ "type": "agentMessage", "id": "item-2", "text": "second turn" }]
                }
            ]
        });
        let rollout = crate::rollout::parse(
            r#"{"timestamp":"2026-07-25T15:08:36.373Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_one","input":"await tools.shell_command({\"command\":\"cargo test\"});","internal_chat_message_metadata_passthrough":{"turn_id":"turn-2"}}}
{"timestamp":"2026-07-25T15:08:38.010Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_one","output":[{"type":"input_text","text":"Exit code: 0\nWall time: 0.5 seconds\nOutput:\nok\n"}]}}"#,
        );

        state.load_history(&thread, Some(&rollout));

        assert_eq!(state.committed.len(), 3);
        assert_eq!(state.committed[0].body, "first turn");
        assert_eq!(state.committed[1].body, "second turn");
        assert!(state.committed[2].title.starts_with("Shell ·"));
    }

    #[test]
    fn turn_plan_updates_do_not_add_transcript_cards() {
        let mut state = test_state();

        state.handle_notification(
            "turn/plan/updated",
            &json!({
                "threadId": "thread",
                "turnId": "turn-1",
                "explanation": "범위를 확인했습니다.",
                "plan": [
                    { "step": "현재 구현 확인", "status": "completed" },
                    { "step": "표시 동작 구현", "status": "inProgress" },
                    { "step": "회귀 테스트", "status": "pending" }
                ]
            }),
        );

        assert!(state.committed.is_empty());
        assert_eq!(state.plan_summary.as_ref().map(|summary| summary.steps.len()), Some(3));
        assert_eq!(
            state.plan_summary.as_ref().and_then(|summary| summary.explanation.as_deref()),
            Some("범위를 확인했습니다.")
        );
        state.prepare_resume();
        assert!(state.plan_summary.is_none());
    }

    #[test]
    fn completed_plan_step_keeps_its_elapsed_time() {
        let mut state = test_state();
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "inProgress" }] }),
        );
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );
        let elapsed = state.plan_summary.as_ref().unwrap().steps[0].elapsed;

        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );

        assert_eq!(state.plan_summary.as_ref().unwrap().steps[0].elapsed, elapsed);
        assert!(elapsed.is_some());
    }

    #[test]
    fn plan_expansion_survives_next_prompt_and_plan_update() {
        let mut state = test_state();
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "inProgress" }] }),
        );
        state.toggle_plan_summary();

        state.turn_input("next prompt".to_owned());
        state.handle_notification(
            "turn/plan/updated",
            &json!({ "plan": [{ "step": "check", "status": "completed" }] }),
        );

        assert!(state.plan_summary.as_ref().is_some_and(|summary| summary.expanded));
    }

    #[test]
    fn resumed_plan_turns_in_progress_steps_into_pending() {
        let mut state = test_state();
        state.restore_plan_snapshot(&PlanSnapshot {
            explanation: None,
            steps: vec![
                crate::rollout::PlanStepSnapshot { text: "완료 작업".to_owned(), status: "completed".to_owned(), elapsed_ms: Some(1_000) },
                crate::rollout::PlanStepSnapshot { text: "진행 중이던 작업".to_owned(), status: "in_progress".to_owned(), elapsed_ms: Some(2_000) },
            ],
        });

        let steps = &state.plan_summary.expect("restored plan").steps;
        assert_eq!(steps[0].status, PlanStepStatus::Completed);
        assert_eq!(steps[1].status, PlanStepStatus::Pending);
        assert_eq!(steps[1].elapsed, Some(Duration::from_secs(2)));
    }

    #[test]
    fn command_output_arrives_without_its_escape_sequences() {
        // A pty-backed shell colours its errors and sets the window title; both
        // would otherwise be measured as visible columns.
        let raw = "\x1b[31mfatal\x1b[0m: no\n\x1b]0;title\x07plain\n\x1b[1;32mok\x1b[m\n";

        assert_eq!(strip_ansi(raw), "fatal: no\nplain\nok\n");
    }

    #[test]
    fn stripping_escape_sequences_leaves_ordinary_text_alone() {
        let plain = "C:\\Users\\x\\SKILL.md' because it does not exist.\n  ~~~~~\n";

        assert_eq!(strip_ansi(plain), plain);
    }

    #[test]
    fn shift_tab_leaves_fixed_full_access_unchanged() {
        let mut state = test_state();
        let action = state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert!(matches!(action, Action::None));
        assert_eq!(state.permission_mode(), PermissionMode::FullAccess);
        assert_eq!(state.permission_profile(), ":danger-full-access");
    }

    #[test]
    fn shift_tab_still_cycles_while_a_slash_command_is_being_typed() {
        let mut state = test_state();
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

        // Numbered rows open with the first choice highlighted.
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
        for (method, params, expected) in [(
            "model/rerouted",
            json!({ "fromModel": "Sol", "toModel": "Luna" }),
            "Sol → Luna로 전환됨",
        )] {
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

    /// The server rescans skills and apps on every file touch, so a notice there
    /// would never stop flashing. The catalogues still reload in the event loop.
    #[test]
    fn a_skill_or_app_rescan_is_silent() {
        for (method, params) in [
            ("skills/changed", json!({})),
            ("app/list/updated", json!({ "apps": [] })),
        ] {
            let mut state = test_state();

            state.handle_notification(method, &params);

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
    fn shell_display_mode_starts_from_the_configured_default() {
        assert_eq!(
            parse_shell_display_mode("shell_display_mode = \"expand\"\n"),
            Some(ShellDisplayMode::Expand)
        );
        assert_eq!(
            parse_shell_display_mode("shell_display_mode = \"hide\" # compact transcript\n"),
            Some(ShellDisplayMode::Hide)
        );
        assert_eq!(
            parse_shell_display_mode("[ui]\nshell_display_mode = \"hide\"\n"),
            None
        );
        assert_eq!(
            parse_shell_display_mode("shell_display_mode = \"other\"\n"),
            None
        );
    }

    #[test]
    fn status_line_fields_use_the_configured_booleans() {
        assert_eq!(
            parse_status_line_field(
                "status_line_five_hour = \"true\" # keep it visible\n",
                StatusLineField::FiveHour,
            ),
            Some(true)
        );
        assert_eq!(
            parse_status_line_field(
                "[ui]\nstatus_line_weekly = false\n",
                StatusLineField::Weekly,
            ),
            None
        );
    }

    #[test]
    fn status_command_reports_the_active_permission_profile() {
        let mut state = test_state();
        state.run_slash_command("/status");

        let body = &state.committed.last().expect("status block").body;
        assert!(body.contains("permissions: Full Access (:danger-full-access)"));
    }

    #[test]
    fn statusline_command_toggles_individual_status_fields_with_space() {
        let mut state = test_state();
        state.status_line_settings = StatusLineSettings::default();

        state.run_slash_command("/statusline");

        let overlay = state.overlay_view().expect("status line picker");
        assert_eq!(overlay.title, "Status line");
        assert_eq!(
            overlay
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "☑ Model",
                "☑ Effort",
                "☑ Context",
                "☑ 5h limit",
                "☑ Weekly limit",
            ]
        );
        assert!(overlay.lines[0].selected);

        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        let overlay = state.overlay_view().expect("status line picker stays open");
        assert_eq!(overlay.lines[0].text, "☐ Model");
        assert_eq!(state.status_line().model, None);

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let overlay = state.overlay_view().expect("status line picker stays open");
        assert_eq!(overlay.lines[1].text, "☐ Effort");
        assert_eq!(state.status_line().effort, None);

        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(state.overlay_view().is_none());
    }

    #[test]
    fn clicking_a_statusline_checkbox_toggles_its_field() {
        let mut state = test_state();
        state.run_slash_command("/statusline");

        let action = state.click_overlay_row(1);

        assert!(matches!(
            action,
            Action::PersistStatusLine {
                key_path: "status_line_effort",
                enabled: false,
            }
        ));
        let overlay = state.overlay_view().expect("status line picker stays open");
        assert_eq!(overlay.lines[1].text, "☐ Effort");
        assert!(overlay.lines[1].selected);
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
    fn shell_diff_and_fast_commands_open_selectable_setting_pickers() {
        let mut state = test_state();

        assert!(matches!(state.run_slash_command("/shell"), Action::None));
        let shell = state.overlay_view().expect("shell picker");
        assert_eq!(shell.title, "Shell");
        assert_eq!(
            shell.slider.as_ref().expect("steps").efforts,
            ["Hide", "Collapse", "Expand"]
        );
        assert!(matches!(
            state.click_effort_step(2),
            Action::PersistShellDisplayMode(ShellDisplayMode::Expand)
        ));

        assert!(matches!(state.run_slash_command("/diff"), Action::None));
        assert_eq!(state.overlay_view().expect("diff picker").title, "Diff");
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::PersistDiffDisplayMode(DiffDisplayMode::Hide)
        ));

        assert!(matches!(state.run_slash_command("/fast"), Action::None));
        let fast = state.overlay_view().expect("fast picker");
        assert_eq!(fast.title, "Fast");
        assert_eq!(fast.slider.as_ref().expect("steps").efforts, ["On", "Off"]);
        assert!(matches!(state.click_effort_step(0), Action::SetFast(true)));
    }

    #[test]
    fn fast_mode_updates_the_badge_and_reports_the_switch() {
        let mut state = test_state();

        state.set_fast_mode(true);

        assert!(state.fast_mode);
        assert!(state.composer_mode().fast_mode);
        assert!(state.transient_status.is_none());
        let on = state
            .committed
            .iter()
            .find(|block| block.title.starts_with("✓ Fast mode"))
            .expect("fast mode notice");
        assert_eq!(on.title, "✓ Fast mode On");

        state.set_fast_mode(false);

        assert_eq!(
            state
                .committed
                .iter()
                .filter(|block| block.title.starts_with("✓ Fast mode"))
                .last()
                .map(|block| block.title.as_str()),
            Some("✓ Fast mode Off")
        );
    }

    #[test]
    fn composer_badge_carries_fixed_access_and_fast_tier() {
        let mut state = test_state();
        state.set_fast_mode(false);

        let badge = state.composer_mode();

        assert_eq!(badge.label, "Full Access");
        assert_eq!(badge.response_length, "Short");
        assert!(!badge.fast_mode);

        state.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(state.composer_mode().label, "Full Access");
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
    fn renderer_is_not_a_slash_command() {
        let mut state = test_state();

        state.editor.set_text("/rend");
        assert!(state.matching_slash_commands().is_empty());

        assert!(matches!(
            state.run_slash_command("/renderer inline"),
            Action::None
        ));
        let error = state.committed.last().expect("unknown-command error");
        assert_eq!(error.title, "알 수 없는 명령");
        assert!(error.body.contains("/renderer"));
    }

    #[test]
    fn theme_command_supports_picker_and_direct_selection() {
        let mut state = test_state();

        assert!(matches!(state.run_slash_command("/theme"), Action::None));
        let overlay = state.overlay_view().expect("theme picker");
        assert_eq!(overlay.title, "Theme");
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
    fn resume_picker_uses_a_compact_time_first_ten_row_window() {
        let sessions = (0..12)
            .map(|index| SessionInfo {
                id: format!("session-{index}"),
                name: Some(format!("Session {index}")),
                preview: String::new(),
                cwd: r"C:\work\current".to_owned(),
                updated_at: 0,
            })
            .collect();
        let picker = SessionPicker::new(sessions, r"C:\work\current".to_owned(), None);

        let view = picker.overlay_view();

        assert!(matches!(view.style, OverlayStyle::CompactPanel));
        assert_eq!(view.lines.len(), 10);
        assert!(view.lines[0].text.starts_with("unknown"));
        assert!(view.lines[0].text.contains("Session 0"));
        assert!(view.input_label.is_empty());
    }

    #[test]
    fn resume_picker_reduces_rows_before_short_terminal_frame_is_clipped() {
        assert_eq!(resume_picker_rows(11), 1);
        assert_eq!(resume_picker_rows(15), 4);
        assert_eq!(resume_picker_rows(40), RESUME_PICKER_ROWS);
    }

    /// One click resumes, the same as Enter on that row. The row index counts
    /// from the top of the window, so a scrolled list must still land on the
    /// session that was painted there.
    #[test]
    fn clicking_a_resume_row_selects_the_session_painted_there() {
        let sessions = (0..12)
            .map(|index| SessionInfo {
                id: format!("session-{index}"),
                name: Some(format!("Session {index}")),
                preview: String::new(),
                cwd: r"C:\work\current".to_owned(),
                updated_at: 0,
            })
            .collect();
        let mut picker = SessionPicker::new(sessions, r"C:\work\current".to_owned(), None);

        assert!(matches!(
            picker.click_row(2),
            SessionPickerResult::Select(id) if id == "session-2"
        ));

        // Walking past the tenth row scrolls the window, and the second row is
        // then the second session of the window rather than of the list.
        for _ in 0..11 {
            picker.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let painted = picker.overlay_view().lines[1].text.clone();
        let SessionPickerResult::Select(id) = picker.click_row(1) else {
            panic!("a painted row resumes the session on it");
        };
        assert!(painted.contains(&format!("Session {}", id.trim_start_matches("session-"))));

        // Past the end of the window there is nothing painted to resume.
        assert!(matches!(picker.click_row(10), SessionPickerResult::None));
    }

    #[test]
    fn resume_picker_keeps_all_project_folders_on_one_row() {
        let mut picker = SessionPicker::new(
            vec![SessionInfo {
                id: "other".to_owned(),
                name: Some("Other project".to_owned()),
                preview: String::new(),
                cwd: r"C:\work\other".to_owned(),
                updated_at: 0,
            }],
            r"C:\work\current".to_owned(),
            None,
        );
        picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

        let view = picker.overlay_view();

        assert!(view.lines[0].text.contains(r"C:\work\other"));
        assert!(!view.lines[0].text.contains('\n'));
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
    fn model_and_effort_picker_copy_is_not_duplicated_in_the_body() {
        let mut state = test_state();

        let _ = state.run_slash_command("/model");
        let model = state.overlay_view().expect("model picker");
        assert_eq!(model.title, "Model");
        assert!(model.slider.is_some());
        assert!(
            model.lines.iter().all(|line| line.text != "Effort"),
            "Effort must not be a standalone model-picker row"
        );

        state.pending = None;
        let _ = state.run_slash_command("/effort");
        let effort = state.overlay_view().expect("effort picker");
        assert_eq!(effort.title, "Effort");
        assert!(
            effort.lines.is_empty(),
            "the effort picker must not repeat the selected model name"
        );
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
        // Picking a model now asks how long it should last before applying.
        state.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(state.selected_model_display_name(), "GPT-5.6-Terra");
        assert_eq!(
            state.overlay_view().expect("scope picker").title,
            "Apply to"
        );
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.selected_model_display_name(), "GPT-5.6-Sol");
        assert!(state.overlay_view().is_none());
    }

    #[test]
    fn choosing_the_default_scope_persists_the_model_and_effort() {
        let mut state = test_state();
        state.run_slash_command("/model");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // Second choice in the scope list writes the config.
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

        assert!(matches!(
            action,
            Action::PersistModelDefault { ref model, ref effort }
                if model == "gpt-5.6-sol" && effort == "high"
        ));
        assert!(state.overlay_view().is_none());
    }

    #[test]
    fn model_scope_options_are_fully_english() {
        let mut state = test_state();
        state.run_slash_command("/model");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let overlay = state.overlay_view().expect("scope picker");
        let options = &overlay.lines[2..];

        assert!(options[0].text.contains("This session only"));
        assert!(options[1].text.contains("Set as default"));
        assert!(
            options
                .iter()
                .all(|line| !line.text.chars().any(|ch| ('가'..='힣').contains(&ch)))
        );
    }

    #[test]
    fn model_scope_descriptions_share_a_column() {
        let mut state = test_state();
        state.run_slash_command("/model");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let overlay = state.overlay_view().expect("scope picker");
        let options = &overlay.lines[2..];

        assert_eq!(
            options[0].text.find("Returns to"),
            options[1].text.find("Saves to")
        );
    }

    #[test]
    fn the_effort_slider_carries_every_tier_and_clamps_the_selection() {
        let model = test_model("gpt-5.6-sol", "GPT-5.6-Sol", true);

        let slider = effort_slider(&model, 2);
        assert_eq!(
            slider.efforts,
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(slider.selected, 2);
        // A stale index from a model with more tiers must not run off the end.
        assert_eq!(effort_slider(&model, 99).selected, 5);
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

    #[test]
    fn resuming_a_completed_turn_keeps_its_completion_label() {
        let model = test_model("gpt-5.6-sol", "GPT-5.6-Sol", true);
        let mut state = AppState::new(
            "main-thread".to_owned(),
            "cwd".to_owned(),
            "account".to_owned(),
            vec![model],
            "gpt-5.6-sol",
            Some("high"),
        );

        state.load_history(
            &json!({
                "turns": [{
                    "startedAt": 1_784_992_100_i64,
                    "completedAt": 1_784_992_165_i64,
                    "items": []
                }]
            }),
            None,
        );

        assert_eq!(
            state.activity().as_deref(),
            Some("Completed (1m 5s)")
        );
    }

    #[test]
    fn activity_shows_only_the_elapsed_turn_time() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6-Terra", false));
        state.note_pending_turn_model("gpt-5.6-terra");
        state.note_pending_turn_effort("high");
        state.handle_notification("turn/started", &json!({ "turn": { "id": "turn-1" } }));
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(10));

        assert!(state
            .activity()
            .is_some_and(|activity| activity.starts_with("Working.. (10s)")));

        state.select_model_and_effort("gpt-5.6-sol", Some("medium"));
        state.handle_notification("turn/completed", &json!({}));
        assert_eq!(
            state.activity().as_deref(),
            Some("Completed (10s)")
        );
    }

    #[test]
    fn activity_color_model_tracks_a_model_change_immediately() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6-Terra", false));
        state.note_pending_turn_model("gpt-5.6-sol");
        state.handle_notification("turn/started", &json!({ "turn": { "id": "turn-1" } }));

        state.apply_model(1, Some("high"));
        assert_eq!(state.activity_model().as_deref(), Some("gpt-5.6-terra"));

        state.handle_notification("turn/completed", &json!({}));
        assert_eq!(state.activity_model().as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn interrupted_turn_activity_is_not_labeled_completed() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Esc)),
            Action::Interrupt
        ));
        state.handle_notification("turn/completed", &json!({}));

        assert!(
            state
                .activity()
                .is_some_and(|activity| activity == "X Interrupted")
        );
    }

    #[test]
    fn interrupted_turn_activity_freezes_at_the_interrupt_time() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(10));

        state.handle_key(KeyEvent::from(KeyCode::Esc));
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(20));

        assert_eq!(
            state.activity().as_deref(),
            Some("X Interrupted")
        );

        state.handle_notification("turn/completed", &json!({}));
        assert_eq!(
            state.activity().as_deref(),
            Some("X Interrupted")
        );
    }

    #[test]
    fn copy_notice_replaces_the_activity_without_using_the_composer_notice() {
        let mut state = test_state();
        state.busy = true;
        state.turn_started_at = Some(Instant::now() - Duration::from_secs(10));

        state.set_copy_notice();

        assert_eq!(state.activity().as_deref(), Some("• Copied to clipboard"));
        assert_eq!(state.view().composer_notice, None);
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
    fn plan_notification_hides_the_welcome_panel() {
        let mut state = test_state();
        assert!(state.view().welcome.is_some());

        state.handle_notification(
            "turn/plan/updated",
            &json!({
                "plan": [{ "step": "check", "status": "inProgress" }]
            }),
        );

        assert!(state.view().welcome.is_none());
    }

    #[test]
    fn busy_empty_composer_shows_steer_and_queue_hint() {
        let state = busy_state_with_live_turn();

        assert_eq!(
            state.view().composer_placeholder,
            "Enter: steer · Tab: queue"
        );
    }

    #[test]
    fn tab_during_a_turn_queues_the_composer_text() {
        let mut state = busy_state_with_live_turn();
        state.editor.set_text("next prompt");

        let action = state.handle_key(KeyEvent::from(KeyCode::Tab));

        assert!(matches!(action, Action::None));
        assert!(state.editor.is_empty());
        assert_eq!(state.queued_prompts.front().map(String::as_str), Some("next prompt"));
    }

    #[test]
    fn queued_prompt_starts_after_the_active_turn_completes() {
        let mut state = busy_state_with_live_turn();
        state.busy = false;
        state.turn_id = None;

        let action = state.start_queued_prompt("next prompt".to_owned());

        assert!(matches!(action, Action::Submit(text) if text == "next prompt"));
        assert!(state.busy);
    }

    #[test]
    fn queued_prompt_can_be_removed_by_its_display_index() {
        let mut state = busy_state_with_live_turn();
        state.queued_prompts = ["first", "second", "third"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        assert!(state.remove_queued_prompt(1));
        assert_eq!(
            state.queued_prompts.into_iter().collect::<Vec<_>>(),
            ["first", "third"]
        );
    }

    #[test]
    fn ctrl_c_interrupts_then_quits_an_active_turn() {
        let mut state = test_state();
        state.set_turn_started("turn-1".to_owned());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert!(matches!(state.handle_key(ctrl_c), Action::Interrupt));
        assert_eq!(
            state.activity().as_deref(),
            Some("• Ctrl+C 한 번 더 누르면 종료합니다.")
        );
        assert!(matches!(state.handle_key(ctrl_c), Action::Quit));
    }

    #[test]
    fn status_metadata_parses_usage_and_fast_mode() {
        let usage = json!({
            "five_hour": { "used_percent": 12.4 },
            "weekly": { "used_percent": 70 }
        });

        assert_eq!(parse_codex_usage(&usage), (Some(12), Some(70)));
        assert!(parse_fast_mode(
            "service_tier = \"fast\"\n[features]\nexample = true"
        ));
        assert!(!parse_fast_mode("service_tier = \"default\""));
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
    fn turn_input_preserves_an_image_path_as_raw_text() {
        let mut state = test_state();
        let path = r"C:\Users\me\AppData\Local\Temp\clipboard.png";

        let input = state.turn_input(format!("inspect {path}"));

        assert_eq!(input.len(), 1);
        assert_eq!(
            input[0].get("text").and_then(Value::as_str),
            Some("inspect C:\\Users\\me\\AppData\\Local\\Temp\\clipboard.png")
        );
    }

    #[test]
    fn turn_input_sends_explicitly_attached_images_as_local_image_items() {
        let mut state = test_state();
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());

        let input = state.turn_input("describe this".to_owned());

        assert_eq!(input.len(), 2);
        assert_eq!(
            input[0].get("text").and_then(Value::as_str),
            Some("describe this")
        );
        assert_eq!(
            input[1].get("type").and_then(Value::as_str),
            Some("localImage")
        );
        assert_eq!(
            input[1].get("path").and_then(Value::as_str),
            Some(r"C:\Temp\clipboard-image.bmp")
        );
    }

    #[test]
    fn composer_backspace_removes_an_explicit_image_attachment() {
        let mut state = test_state();
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());

        state.handle_key(KeyEvent::from(KeyCode::Backspace));

        assert_eq!(state.composer_image_count(), 0);
    }

    #[test]
    fn composer_ctrl_backspace_removes_an_explicit_image_attachment() {
        let mut state = test_state();
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());

        state.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));

        assert_eq!(state.composer_image_count(), 0);
    }

    #[test]
    fn composer_arrows_cross_an_image_attachment_as_one_block() {
        let mut state = test_state();
        state.editor.set_text("before");
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());
        assert_eq!(state.editor.attachment_before_cursor(), Some(0));

        state.handle_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(state.editor.attachment_at_cursor(), Some(0));

        state.handle_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.editor.attachment_before_cursor(), Some(0));
        assert_eq!(state.editor.text(), "before");
    }

    #[test]
    fn composer_ctrl_backspace_control_character_deletes_a_word() {
        let mut state = test_state();
        state.handle_paste("first second");

        state.handle_key(KeyEvent::from(KeyCode::Char('\u{8}')));

        assert_eq!(state.editor.text(), "first ");
    }

    #[test]
    fn composer_ctrl_backspace_repeat_does_not_delete_another_korean_word() {
        let mut state = test_state();
        state.handle_paste("첫째 둘째");
        let mut first = KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL);
        first.kind = KeyEventKind::Press;
        state.handle_key(first);

        let mut repeat = KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL);
        repeat.kind = KeyEventKind::Repeat;
        state.handle_key(repeat);

        assert_eq!(state.editor.text(), "첫째 ");
    }

    fn composer_completion_state() -> AppState {
        let mut state = test_state();
        state.update_skills(&json!({
            "data": [{
                "cwd": "cwd",
                "errors": [],
                "skills": [{
                    "name": "review",
                    "path": "C:/skills/review/SKILL.md",
                    "description": "Review a change",
                    "enabled": true,
                    "scope": "user"
                }]
            }]
        }));
        state.update_plugins(&json!({
            "marketplaces": [{
                "name": "openai-bundled",
                "plugins": [{
                    "id": "browser-use@openai-bundled",
                    "name": "browser-use",
                    "description": "Control a browser",
                    "installed": true,
                    "enabled": true,
                    "interface": { "displayName": "Browser Use" }
                }]
            }]
        }));
        state.update_apps(&json!({
            "data": [{
                "id": "calendar",
                "name": "Calendar",
                "description": "Read calendar events",
                "isAccessible": true,
                "isEnabled": true
            }]
        }));
        state.update_workspace_entries(vec![
            crate::completion::CompletionCandidate::new(
                crate::completion::CompletionKind::Directory,
                "src",
                "",
                "src",
            ),
            crate::completion::CompletionCandidate::new(
                crate::completion::CompletionKind::File,
                "src/main.rs",
                "",
                "src/main.rs",
            ),
        ]);
        state
    }

    #[test]
    fn composer_completion_catalogs_match_current_codex() {
        let mut state = composer_completion_state();
        state.editor.set_text("$");
        let dollar = state
            .view()
            .suggestions
            .iter()
            .map(|suggestion| suggestion.category.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            dollar,
            [
                Some("Plugin".to_owned()),
                Some("Skill".to_owned()),
                Some("App".to_owned())
            ]
        );

        state.editor.set_text("@");
        let at = state
            .view()
            .suggestions
            .iter()
            .map(|suggestion| suggestion.category.clone())
            .collect::<Vec<_>>();
        assert_eq!(at, [Some("Plugin".to_owned()), Some("Skill".to_owned())]);

        state.editor.set_text("@src");
        let filesystem = state
            .view()
            .suggestions
            .iter()
            .map(|suggestion| suggestion.category.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            filesystem,
            [Some("Dir".to_owned()), Some("File".to_owned())]
        );
    }

    #[test]
    fn composer_completion_enter_inserts_a_skill_without_submitting() {
        let mut state = composer_completion_state();
        state.editor.set_text("@rev");

        let action = state.handle_key(KeyEvent::from(KeyCode::Enter));

        assert!(matches!(action, Action::None));
        assert_eq!(state.editor.text(), "$review ");
    }

    #[test]
    fn composer_completion_replaces_only_the_active_mid_draft_file_token() {
        let mut state = composer_completion_state();
        state.editor.set_text("open @mai later");
        for _ in 0..6 {
            state.editor.move_left();
        }

        let action = state.handle_key(KeyEvent::from(KeyCode::Tab));

        assert!(matches!(action, Action::None));
        assert_eq!(state.editor.text(), "open src/main.rs  later");
    }

    #[test]
    fn composer_escape_clears_the_prompt_and_attachments() {
        let mut state = composer_completion_state();
        state.editor.set_text("$rev");
        state.attach_local_image(r"C:\Temp\clipboard-image.bmp".to_owned());
        assert!(!state.view().suggestions.is_empty());

        assert!(matches!(
            state.handle_key(KeyEvent::from(KeyCode::Esc)),
            Action::None
        ));
        assert!(state.editor.is_empty());
        assert_eq!(state.composer_image_count(), 0);
        assert!(state.view().suggestions.is_empty());
    }

    #[test]
    fn a_large_paste_stays_intact_behind_its_composer_summary() {
        let mut state = test_state();
        state.handle_paste("one\ntwo\nthree\nfour\nfive\nsix");

        assert_eq!(state.editor.paste_summary_lines(), Some(6));
        assert_eq!(state.editor.text(), "one\ntwo\nthree\nfour\nfive\nsix");

        state.handle_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(state.editor.paste_summary_lines(), Some(6));
    }

    #[test]
    fn typing_after_a_large_paste_keeps_the_summary_and_preserves_submit_text() {
        let mut state = test_state();
        state.handle_paste("one\ntwo\nthree\nfour\nfive\nsix");
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        state.handle_key(KeyEvent::from(KeyCode::Char('!')));

        assert_eq!(state.editor.paste_summary_lines(), Some(6));
        assert_eq!(
            state.editor.take_for_submit().as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix\n!")
        );
    }

    #[test]
    fn deleting_tail_text_after_a_large_paste_keeps_the_summary() {
        let mut state = test_state();
        state.handle_paste("one\ntwo\nthree\nfour\nfive\nsix");
        state.handle_key(KeyEvent::from(KeyCode::Char('!')));
        state.handle_key(KeyEvent::from(KeyCode::Backspace));

        assert_eq!(state.editor.paste_summary_lines(), Some(6));
        assert_eq!(
            state.editor.take_for_submit().as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix")
        );
    }

    #[test]
    fn composer_completion_empty_mode_stays_open_for_mode_switching() {
        let mut state = composer_completion_state();
        state.editor.set_text("@rev");

        state.handle_key(KeyEvent::from(KeyCode::Right));
        let empty_mode = state.view().suggestions;
        assert_eq!(empty_mode.len(), 1);
        assert_eq!(empty_mode[0].command, "No matches");

        state.handle_key(KeyEvent::from(KeyCode::Right));
        assert!(
            state
                .view()
                .suggestions
                .iter()
                .any(|suggestion| suggestion.command == "review")
        );
    }

    #[test]
    fn composer_completion_shell_like_query_requires_a_catalog_match() {
        let mut state = composer_completion_state();
        state.editor.set_text("$1missing");
        assert!(state.view().suggestions.is_empty());

        state.update_skills(&json!({
            "data": [{
                "cwd": "cwd",
                "errors": [],
                "skills": [{
                    "name": "1password",
                    "path": "C:/skills/1password/SKILL.md",
                    "description": "Use 1Password",
                    "enabled": true,
                    "scope": "user"
                }]
            }]
        }));
        state.editor.set_text("$1p");
        assert!(
            state
                .view()
                .suggestions
                .iter()
                .any(|suggestion| suggestion.command == "1password")
        );
    }

    #[test]
    fn mention_tokens_do_not_treat_email_addresses_as_plugins() {
        assert_eq!(
            mention_triggers("mail foo@sample.com, then use @sample"),
            vec![('@', "sample".to_owned())]
        );
    }

    #[test]
    fn mention_submission_keeps_sigil_categories_separate() {
        let mut state = test_state();
        state.update_skills(&json!({
            "data": [{
                "skills": [{
                    "name": "calendar",
                    "path": "C:/skills/calendar/SKILL.md",
                    "enabled": true
                }]
            }]
        }));
        state.update_plugins(&json!({
            "marketplaces": [{
                "name": "market",
                "plugins": [{
                    "id": "calendar@market",
                    "name": "calendar",
                    "installed": true,
                    "enabled": true
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

        let dollar = state.turn_input("$calendar".to_owned());
        assert_eq!(dollar.len(), 2);
        assert_eq!(dollar[1].get("type").and_then(Value::as_str), Some("skill"));

        let at = state.turn_input("@calendar".to_owned());
        assert_eq!(at.len(), 2);
        assert_eq!(at[1].get("type").and_then(Value::as_str), Some("mention"));
        assert_eq!(
            at[1].get("path").and_then(Value::as_str),
            Some("plugin://calendar@market")
        );
    }

    #[test]
    fn selected_app_binding_wins_over_a_same_named_skill() {
        let mut state = test_state();
        state.update_skills(&json!({
            "data": [{
                "skills": [{
                    "name": "calendar",
                    "path": "C:/skills/calendar/SKILL.md",
                    "enabled": true
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
        state.editor.set_text("$cal");
        state.handle_key(KeyEvent::from(KeyCode::Down));
        state.handle_key(KeyEvent::from(KeyCode::Enter));

        let input = state.turn_input(state.editor.text());
        assert_eq!(input.len(), 2);
        assert_eq!(
            input[1].get("path").and_then(Value::as_str),
            Some("app://calendar")
        );
    }

    #[test]
    fn edited_completion_does_not_keep_its_selected_binding() {
        let mut state = test_state();
        state.update_apps(&json!({
            "data": [{
                "id": "calendar",
                "name": "Calendar",
                "isAccessible": true,
                "isEnabled": true
            }]
        }));
        state.editor.set_text("$cal");
        state.handle_key(KeyEvent::from(KeyCode::Enter));
        state.handle_key(KeyEvent::from(KeyCode::Backspace));
        state.handle_key(KeyEvent::from(KeyCode::Char('x')));

        let input = state.turn_input(state.editor.text());

        assert_eq!(input.len(), 1);
    }

    #[test]
    fn identical_tokens_keep_their_individual_selected_bindings() {
        let mut state = test_state();
        state.update_skills(&json!({
            "data": [{
                "skills": [{
                    "name": "calendar",
                    "path": "C:/skills/calendar/SKILL.md",
                    "enabled": true
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
        state.editor.set_text("$cal");
        state.handle_key(KeyEvent::from(KeyCode::Enter));
        state.handle_paste("$cal");
        state.handle_key(KeyEvent::from(KeyCode::Down));
        state.handle_key(KeyEvent::from(KeyCode::Enter));

        let input = state.turn_input(state.editor.text());
        let paths = input
            .iter()
            .filter_map(|item| item.get("path").and_then(Value::as_str))
            .collect::<Vec<_>>();

        assert_eq!(paths, ["C:/skills/calendar/SKILL.md", "app://calendar"]);
    }

    #[test]
    fn changing_cwd_clears_old_workspace_completions_immediately() {
        let mut state = composer_completion_state();
        state.editor.set_text("@src");
        assert!(
            state
                .view()
                .suggestions
                .iter()
                .any(|suggestion| suggestion.category.as_deref() == Some("File"))
        );

        state.attach_thread(
            "thread-2".to_owned(),
            "C:/other-workspace".to_owned(),
            "gpt-5",
            None,
        );

        assert!(
            state
                .view()
                .suggestions
                .iter()
                .all(|suggestion| !matches!(suggestion.category.as_deref(), Some("File" | "Dir")))
        );
    }

    #[test]
    fn integration_slash_commands_dispatch_app_server_actions() {
        let mut state = test_state();
        assert!(matches!(
            state.run_slash_command("/mcp"),
            Action::OpenMcp(None)
        ));
        assert!(matches!(
            state.run_slash_command("/mcp reconnect"),
            Action::ReconnectMcp
        ));
        assert!(matches!(
            state.run_slash_command("/mcp login github"),
            Action::McpLogin(ref name) if name == "github"
        ));
        assert!(matches!(
            state.run_slash_command("/plugins"),
            Action::OpenPlugins {
                scope: None,
                notice: None
            }
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
    }

    #[test]
    fn marketplace_subcommands_route_through_the_confirmation_step() {
        let mut state = test_state();

        assert!(matches!(
            state.run_slash_command("/plugins marketplace"),
            Action::OpenMarketplaces(None)
        ));
        assert!(matches!(
            state.run_slash_command("/plugins marketplace add owner/repo"),
            Action::ConfirmMarketplaceAdd(ref source) if source == "owner/repo"
        ));
        assert!(matches!(
            state.run_slash_command("/plugins marketplace remove openai-bundled"),
            Action::ConfirmMarketplaceRemove(ref name) if name == "openai-bundled"
        ));
        assert!(matches!(
            state.run_slash_command("/plugins marketplace upgrade"),
            Action::UpgradeMarketplaces
        ));
        // A name is rejected rather than silently ignored, because the server
        // upgrades every git marketplace regardless of what is named.
        assert!(matches!(
            state.run_slash_command("/plugins marketplace upgrade openai-bundled"),
            Action::None
        ));
    }

    /// Adding a marketplace checks out a repository whose hooks can run, so the
    /// confirmation must be a real gate rather than a notice.
    #[test]
    fn adding_a_marketplace_waits_for_an_explicit_confirmation() {
        let mut state = test_state();
        state.confirm_marketplace_add("owner/repo");

        let overlay = state.view().overlay.expect("confirmation overlay");
        assert!(overlay.title.contains("마켓플레이스를 추가"));
        assert!(
            overlay
                .lines
                .iter()
                .any(|line| line.text.contains("신뢰할 수 있는 저장소만")),
            "the trust warning has to be on screen before the checkout"
        );

        let declined = state.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(declined, Action::None));

        state.confirm_marketplace_add("owner/repo");
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Action::AddMarketplace(ref source) if source == "owner/repo"
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
    fn cost_keeps_completed_sol_usage_when_the_next_turn_uses_terra() {
        let mut state = test_state();
        state
            .models
            .push(test_model("gpt-5.6-terra", "GPT-5.6 Terra", false));
        state.note_pending_turn_model("gpt-5.6-sol");
        state.handle_notification("turn/started", &json!({ "turn": { "id": "one" } }));
        state.handle_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": { "total": { "inputTokens": 1_000_000 } }
            }),
        );

        state.apply_model(1, None);
        state.note_pending_turn_model("gpt-5.6-terra");
        state.handle_notification("turn/started", &json!({ "turn": { "id": "two" } }));
        state.handle_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": { "total": { "inputTokens": 2_000_000 } }
            }),
        );

        assert_eq!(state.composer_mode().cost.as_deref(), Some("$7.50"));
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
